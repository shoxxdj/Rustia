mod batch;
mod download;
mod util;
mod wistia;

use std::collections::BTreeMap;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use clap::Parser;
use reqwest::Client;

use download::{download_with_resume, part_path, Options};
use util::{is_channel_url, is_valid_id, link_hash_from_url, safe_filename};

#[derive(Parser)]
#[command(
    name = "wistia-downloader",
    version,
    about = "Download Wistia videos or channels, with automatic resume on interruption"
)]
struct Cli {
    /// Wistia video ID (e.g. j4n8x2m7vw)
    #[arg(short = 'i', long, conflicts_with_all = ["url", "clipboard", "csv"])]
    id: Option<String>,

    /// Wistia page URL (media, share link or channel)
    #[arg(short = 'u', long, conflicts_with_all = ["clipboard", "csv"])]
    url: Option<String>,

    /// HTML snippet from "Copy link" (contains wvideo=)
    #[arg(short = 'c', long, conflicts_with = "csv")]
    clipboard: Option<String>,

    /// CSV file with header "id, folder, title": each video is saved as
    /// <folder>/<title>.mp4 (folders created as needed), using --quality
    #[arg(long, conflicts_with_all = ["output", "list"])]
    csv: Option<PathBuf>,

    /// Output file (single video; default: the video title)
    #[arg(short = 'o', long)]
    output: Option<PathBuf>,

    /// Destination folder for a channel
    #[arg(short = 'd', long, default_value = "wistia_downloads")]
    dir: PathBuf,

    /// Consecutive attempts without progress before giving up (0 = retry forever)
    #[arg(short = 'r', long, default_value_t = 20)]
    retries: u32,

    /// Seconds without data before the connection is restarted
    #[arg(long, default_value_t = 30)]
    stall_timeout: u64,

    /// Answer yes to confirmations (channel URL with a video: download that video only)
    #[arg(short = 'y', long)]
    yes: bool,

    /// On a channel URL that targets a video, download the whole channel without asking
    #[arg(long)]
    all: bool,

    /// Download again even if the final file already exists
    #[arg(long)]
    force: bool,

    /// Quality: original (source file, default), best (best re-encoded MP4),
    /// or a maximum resolution such as 1080, 720p, 540…
    #[arg(short = 'q', long, default_value = "original")]
    quality: wistia::Quality,

    /// List the available renditions of the video without downloading
    #[arg(short = 'l', long)]
    list: bool,
}

struct Ctx {
    client: Client,
    opts: Options,
    force: bool,
    quality: wistia::Quality,
}

#[tokio::main]
async fn main() {
    if let Err(e) = run().await {
        eprintln!("❌ {e:#}");
        std::process::exit(1);
    }
}

fn mib(bytes: u64) -> f64 {
    bytes as f64 / 1_048_576.0
}

async fn run() -> Result<()> {
    let cli = Cli::parse();

    let client = Client::builder()
        .user_agent("Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.6 Safari/605.1.15")
        .connect_timeout(Duration::from_secs(20))
        .tcp_keepalive(Duration::from_secs(30))
        .pool_idle_timeout(Duration::from_secs(60))
        .build()?;

    let ctx = Ctx {
        client,
        opts: Options {
            max_retries: cli.retries,
            stall_timeout: Duration::from_secs(cli.stall_timeout.max(5)),
        },
        force: cli.force,
        quality: cli.quality,
    };

    if let Some(path) = &cli.csv {
        return download_csv(&ctx, path, cli.yes).await;
    }

    let id = if let Some(id) = &cli.id {
        id.clone()
    } else if let Some(snippet) = &cli.clipboard {
        let id = util::video_id_from_html(snippet)
            .context("no Wistia video ID found in the HTML snippet")?;
        println!("Video ID found in HTML snippet: {id}");
        id
    } else if let Some(url) = &cli.url {
        match resolve_from_url(&ctx, &cli, url).await? {
            Some(id) => id,
            None => return Ok(()), // channel handled
        }
    } else {
        bail!("provide --id, --url, --clipboard or --csv (see --help)");
    };

    if !is_valid_id(&id) {
        bail!("invalid video ID: {id:?}");
    }

    if cli.list {
        let assets = wistia::list_assets(&ctx.client, &id).await?;
        if assets.is_empty() {
            bail!("no downloadable rendition found");
        }
        println!("Available renditions for {id}:");
        for a in &assets {
            println!("  {}", a.describe());
        }
        let chosen = wistia::select_asset(&assets, ctx.quality).expect("non-empty list");
        println!("Selected with current --quality: {}", chosen.kind);
        return Ok(());
    }

    let output = match &cli.output {
        Some(p) => p.clone(),
        None => match wistia::video_title(&ctx.client, &id).await {
            Some(title) => {
                let p = PathBuf::from(format!("{}.mp4", safe_filename(&title)));
                println!("Using video title as filename: {}", p.display());
                p
            }
            None => {
                let p = PathBuf::from(format!("{id}.mp4"));
                println!("Using video ID as filename: {}", p.display());
                p
            }
        },
    };

    match download_video(&ctx, &id, &output).await? {
        Outcome::Skipped => println!(
            "⏭️  {} already exists (use --force to download again)",
            output.display()
        ),
        Outcome::Done(size) => println!("✅ {} ({:.2} MiB)", output.display(), mib(size)),
    }
    Ok(())
}

/// Returns the ID of a video to download, or `None` if a channel was handled.
async fn resolve_from_url(ctx: &Ctx, cli: &Cli, url: &str) -> Result<Option<String>> {
    if is_channel_url(url) {
        if cli.list {
            return util::video_id_from_url(url)
                .map(Some)
                .context("--list needs a specific video (URL with wmediaid=)");
        }
        match util::video_id_from_url(url) {
            Some(vid) => {
                println!("Wistia channel page targeting a video ({vid})");
                let whole = if cli.all {
                    true
                } else if cli.yes {
                    false
                } else {
                    ask_channel_or_video(ctx, &vid).await?
                };
                if !whole {
                    return Ok(Some(vid));
                }
            }
            None => println!("Wistia channel page detected"),
        }
        download_channel(ctx, url, &cli.dir, cli.yes).await?;
        return Ok(None);
    }

    if let Some(id) = util::video_id_from_url(url) {
        println!("Video ID found in URL: {id}");
        return Ok(Some(id));
    }

    let hash = link_hash_from_url(url).context("no Wistia ID found in the URL")?;
    let id = wistia::resolve_link_graphql(&ctx.client, url, &hash).await?;
    println!("Video ID found via GraphQL: {id}");
    Ok(Some(id))
}

fn prompt(question: &str) -> Result<String> {
    print!("{question}");
    io::stdout().flush()?;
    let mut line = String::new();
    io::stdin().lock().read_line(&mut line)?;
    Ok(line.trim().to_lowercase())
}

fn confirm(question: &str) -> Result<bool> {
    Ok(matches!(prompt(question)?.as_str(), "y" | "yes"))
}

async fn ask_channel_or_video(ctx: &Ctx, vid: &str) -> Result<bool> {
    let title = wistia::video_title(&ctx.client, vid)
        .await
        .unwrap_or_else(|| vid.to_string());
    println!("\nWhat would you like to download?");
    println!("1) Just this video: \"{title}\" (ID: {vid})");
    println!("2) The entire channel");
    Ok(match prompt("Enter your choice (1 or 2): ")?.as_str() {
        "2" => true,
        "1" => false,
        _ => {
            println!("Invalid choice, downloading the single video.");
            false
        }
    })
}

enum Outcome {
    Skipped,
    Done(u64),
}

async fn download_video(ctx: &Ctx, id: &str, dest: &Path) -> Result<Outcome> {
    if dest.exists() && !part_path(dest).exists() {
        if !ctx.force {
            return Ok(Outcome::Skipped);
        }
        std::fs::remove_file(dest).with_context(|| format!("removing {}", dest.display()))?;
    }
    if let Some(parent) = dest.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }

    println!("🎬 {}", dest.display());
    let client = ctx.client.clone();
    let id = id.to_string();
    let quality = ctx.quality;
    let mut announced = false;
    let size = download_with_resume(&ctx.client, dest, &ctx.opts, move || {
        let client = client.clone();
        let id = id.clone();
        let first = !std::mem::replace(&mut announced, true);
        async move {
            let asset = wistia::resolve_asset(&client, &id, quality).await?;
            if first {
                println!("  Rendition: {}", asset.describe());
            }
            Ok(asset.url)
        }
    })
    .await?;
    Ok(Outcome::Done(size))
}

#[derive(Default)]
struct Summary {
    ok: usize,
    skipped: usize,
    failed: Vec<String>,
}

impl Summary {
    fn print(&self) {
        println!("{}\n📊 Summary", "=".repeat(60));
        println!(
            "✅ Successful: {}\n⏭️  Skipped: {}\n❌ Failed: {}",
            self.ok,
            self.skipped,
            self.failed.len()
        );
        for f in &self.failed {
            println!("   - {f}");
        }
        if !self.failed.is_empty() {
            println!("Run the same command again: finished files are skipped, the others resume where they stopped.");
        }
    }
}

async fn download_channel(ctx: &Ctx, url: &str, dir: &Path, yes: bool) -> Result<()> {
    println!("Fetching channel page…");
    let data = wistia::fetch_channel(&ctx.client, url).await?;
    let videos = wistia::channel_videos(&data);
    if videos.is_empty() {
        bail!("no videos found in channel");
    }

    println!(
        "\n📺 Channel {} — {} video(s)",
        data.hashed_id,
        videos.len()
    );
    let mut by_section: BTreeMap<&str, usize> = BTreeMap::new();
    for v in &videos {
        *by_section.entry(v.section.as_str()).or_default() += 1;
    }
    for (section, n) in &by_section {
        let name = if section.is_empty() {
            "(no section)"
        } else {
            section
        };
        println!("  {name}: {n}");
    }
    println!("Files are named after their section and title (-o is ignored).");

    if !yes && !confirm("\nDownload all videos? (y/N): ")? {
        println!("Download cancelled.");
        return Ok(());
    }

    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    println!("\nDestination: {}/\n{}", dir.display(), "=".repeat(60));

    let mut summary = Summary::default();
    for (i, v) in videos.iter().enumerate() {
        println!("\n[{}/{}] {}", i + 1, videos.len(), v.title);
        let base = if v.section.is_empty() {
            v.title.clone()
        } else {
            format!("{} - {}", v.section, v.title)
        };
        let dest = dir.join(format!("{}.mp4", safe_filename(&base)));

        match download_video(ctx, &v.id, &dest).await {
            Ok(Outcome::Skipped) => {
                println!("⏭️  Already exists: {}", dest.display());
                summary.skipped += 1;
            }
            Ok(Outcome::Done(size)) => {
                println!("✅ Done ({:.2} MiB)", mib(size));
                summary.ok += 1;
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            Err(e) => {
                eprintln!("❌ {e:#}");
                summary.failed.push(v.title.clone());
            }
        }
    }

    summary.print();
    println!("📁 {}/", dir.display());
    Ok(())
}

async fn download_csv(ctx: &Ctx, path: &Path, yes: bool) -> Result<()> {
    let (jobs, warnings) = batch::read_jobs(path)?;
    for w in &warnings {
        eprintln!("⚠️  {w}");
    }
    if jobs.is_empty() {
        bail!("no usable rows in {}", path.display());
    }

    println!(
        "📄 {} — {} video(s), quality: {}",
        path.display(),
        jobs.len(),
        ctx.quality
    );
    if !yes && !confirm("Start downloading? (y/N): ")? {
        println!("Download cancelled.");
        return Ok(());
    }
    println!("{}", "=".repeat(60));

    let mut summary = Summary::default();
    for (i, job) in jobs.iter().enumerate() {
        println!(
            "\n[{}/{}] line {} — {}",
            i + 1,
            jobs.len(),
            job.line,
            job.id
        );
        match download_video(ctx, &job.id, &job.dest).await {
            Ok(Outcome::Skipped) => {
                println!("⏭️  Already exists: {}", job.dest.display());
                summary.skipped += 1;
            }
            Ok(Outcome::Done(size)) => {
                println!("✅ {} ({:.2} MiB)", job.dest.display(), mib(size));
                summary.ok += 1;
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            Err(e) => {
                eprintln!("❌ {e:#}");
                summary
                    .failed
                    .push(format!("line {} ({})", job.line, job.id));
            }
        }
    }

    summary.print();
    Ok(())
}
