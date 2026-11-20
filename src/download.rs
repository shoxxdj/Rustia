//! Interruption-tolerant HTTP download.
//!
//! How it works:
//! - bytes are written to `<dest>.part`, renamed to `<dest>` only once complete;
//! - `<dest>.part.json` stores the total size and the server's ETag/Last-Modified;
//! - every attempt resumes where the `.part` file stopped (`Range: bytes=N-`), guarded by
//!   `If-Range`: if the remote file changed, the server sends 200 and we restart from zero;
//! - a connection that receives nothing for `stall_timeout` is dropped and restarted;
//! - on 403/404/410 (expired URL), a fresh URL is requested from the caller;
//! - the retry counter resets as soon as an attempt makes progress, so a download that keeps
//!   advancing, even slowly and with interruptions, never gives up.
//!
//! Ctrl+C or a crash leaves a consistent `.part` file: re-running the command resumes it.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use futures_util::StreamExt;
use indicatif::{ProgressBar, ProgressStyle};
use reqwest::{header, Client, StatusCode};
use serde::{Deserialize, Serialize};
use tokio::fs::{self, OpenOptions};
use tokio::io::{AsyncWriteExt, BufWriter};
use tokio::time::{sleep, timeout};

#[derive(Clone, Debug)]
pub struct Options {
    /// Consecutive attempts without progress before giving up (0 = never).
    pub max_retries: u32,
    /// Time without receiving data after which the connection is restarted.
    pub stall_timeout: Duration,
}

#[derive(Default, Serialize, Deserialize)]
struct Meta {
    total: Option<u64>,
    etag: Option<String>,
    last_modified: Option<String>,
}

enum Failure {
    /// Temporary network or server error: retry with the same URL.
    Transient(anyhow::Error),
    /// The URL looks invalid or expired: ask for a new one.
    Refresh(anyhow::Error),
    /// Error that retrying will not fix (disk full, 400…).
    Fatal(anyhow::Error),
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(suffix);
    PathBuf::from(s)
}

pub fn part_path(dest: &Path) -> PathBuf {
    with_suffix(dest, ".part")
}

fn meta_path(dest: &Path) -> PathBuf {
    with_suffix(dest, ".part.json")
}

async fn file_len(path: &Path) -> u64 {
    fs::metadata(path).await.map(|m| m.len()).unwrap_or(0)
}

async fn load_meta(path: &Path) -> Meta {
    match fs::read(path).await {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
        Err(_) => Meta::default(),
    }
}

async fn save_meta(path: &Path, meta: &Meta) -> Result<()> {
    let tmp = with_suffix(path, ".tmp");
    fs::write(&tmp, serde_json::to_vec(meta)?).await?;
    fs::rename(&tmp, path).await?;
    Ok(())
}

async fn reset(part: &Path, metap: &Path, meta: &mut Meta) {
    let _ = fs::remove_file(part).await;
    *meta = Meta::default();
    let _ = save_meta(metap, meta).await;
}

fn backoff(failures: u32) -> Duration {
    let secs = 1u64 << failures.saturating_sub(1).min(6);
    Duration::from_secs(secs.min(60))
}

/// Parse `Content-Range: bytes START-END/TOTAL` (TOTAL peut valoir `*`).
fn parse_content_range(headers: &header::HeaderMap) -> Option<(Option<u64>, Option<u64>)> {
    let v = headers.get(header::CONTENT_RANGE)?.to_str().ok()?.trim();
    let rest = v.strip_prefix("bytes")?.trim();
    let (range, total) = rest.split_once('/')?;
    let total = total.trim().parse::<u64>().ok();
    let start = if range.trim() == "*" {
        None
    } else {
        Some(range.split_once('-')?.0.trim().parse::<u64>().ok()?)
    };
    Some((start, total))
}

fn header_string(headers: &header::HeaderMap, name: header::HeaderName) -> Option<String> {
    headers.get(name)?.to_str().ok().map(str::to_owned)
}

fn progress_style(known_total: bool) -> ProgressStyle {
    let tpl = if known_total {
        "  {spinner} [{bar:30}] {bytes}/{total_bytes} {bytes_per_sec} ETA {eta}"
    } else {
        "  {spinner} {bytes} downloaded ({bytes_per_sec})"
    };
    ProgressStyle::with_template(tpl)
        .unwrap()
        .progress_chars("█▓░")
}

/// Downloads to `dest`, resuming automatically after every interruption.
/// `resolve_url` is called again to get a fresh URL if the current one is rejected.
/// Returns the final file size.
pub async fn download_with_resume<F, Fut>(
    client: &Client,
    dest: &Path,
    opts: &Options,
    mut resolve_url: F,
) -> Result<u64>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<String>>,
{
    let part = part_path(dest);
    let metap = meta_path(dest);
    let mut meta = load_meta(&metap).await;

    let already = file_len(&part).await;
    if already > 0 {
        println!(
            "  ↻ Resuming partial download ({:.2} MiB already on disk)",
            already as f64 / 1_048_576.0
        );
    }

    let pb = ProgressBar::new(meta.total.unwrap_or(0));
    pb.set_style(progress_style(meta.total.is_some()));
    pb.enable_steady_tick(Duration::from_millis(200));

    let mut url: Option<String> = None;
    let mut failures: u32 = 0;

    loop {
        let before = file_len(&part).await;

        let err = match &url {
            None => match resolve_url().await {
                Ok(u) => {
                    url = Some(u);
                    continue;
                }
                Err(e) => e.context("could not get the video URL"),
            },
            Some(u) => match attempt(client, u, &part, &metap, &mut meta, opts, &pb).await {
                Ok(()) => {
                    let size = file_len(&part).await;
                    fs::rename(&part, dest)
                        .await
                        .with_context(|| format!("renommage de {}", part.display()))?;
                    let _ = fs::remove_file(&metap).await;
                    pb.finish_and_clear();
                    return Ok(size);
                }
                Err(Failure::Fatal(e)) => {
                    pb.abandon();
                    return Err(e.context(format!(
                        "permanent failure (partial file kept: {})",
                        part.display()
                    )));
                }
                Err(Failure::Refresh(e)) => {
                    url = None;
                    e
                }
                Err(Failure::Transient(e)) => e,
            },
        };

        if file_len(&part).await > before {
            failures = 0; // the attempt made progress
        }
        failures += 1;

        if opts.max_retries != 0 && failures > opts.max_retries {
            pb.abandon();
            return Err(err.context(format!(
                "giving up after {} attempts without progress — run the command again to resume ({})",
                opts.max_retries,
                part.display()
            )));
        }

        let delay = backoff(failures);
        let limit = if opts.max_retries == 0 {
            "∞".to_string()
        } else {
            opts.max_retries.to_string()
        };
        pb.println(format!(
            "  ⚠️  {err:#} — retrying in {}s ({failures}/{limit})",
            delay.as_secs()
        ));
        sleep(delay).await;
    }
}

async fn attempt(
    client: &Client,
    url: &str,
    part: &Path,
    metap: &Path,
    meta: &mut Meta,
    opts: &Options,
    pb: &ProgressBar,
) -> Result<(), Failure> {
    let mut offset = file_len(part).await;

    if offset > 0 {
        if let Some(total) = meta.total {
            if offset == total {
                return Ok(());
            }
            if offset > total {
                pb.println("  ⚠️  Partial file larger than expected, restarting from zero");
                reset(part, metap, meta).await;
                offset = 0;
            }
        }
    }

    let mut req = client.get(url);
    if offset > 0 {
        req = req.header(header::RANGE, format!("bytes={offset}-"));
        if let Some(v) = meta.etag.as_ref().or(meta.last_modified.as_ref()) {
            req = req.header(header::IF_RANGE, v);
        }
    }

    let resp = match timeout(opts.stall_timeout, req.send()).await {
        Err(_) => return Err(Failure::Transient(anyhow!("no response from server"))),
        Ok(Err(e)) => return Err(Failure::Transient(anyhow!(e).context("request failed"))),
        Ok(Ok(r)) => r,
    };

    let status = resp.status();
    let append = match status {
        StatusCode::PARTIAL_CONTENT => {
            let (start, total) = parse_content_range(resp.headers()).ok_or_else(|| {
                Failure::Transient(anyhow!("206 response without a valid Content-Range"))
            })?;
            if start != Some(offset) {
                reset(part, metap, meta).await;
                return Err(Failure::Transient(anyhow!(
                    "server returned a different range than requested, restarting"
                )));
            }
            if total.is_some() {
                meta.total = total;
            }
            true
        }
        StatusCode::OK => {
            if offset > 0 {
                pb.println(
                    "  ⚠️  Resume refused (no Range support or file changed), restarting from zero",
                );
            }
            offset = 0;
            meta.total = resp.content_length();
            false
        }
        StatusCode::RANGE_NOT_SATISFIABLE => {
            let total = parse_content_range(resp.headers()).and_then(|(_, t)| t);
            if total == Some(offset) {
                meta.total = total;
                return Ok(());
            }
            reset(part, metap, meta).await;
            return Err(Failure::Transient(anyhow!(
                "range not satisfiable (416), restarting"
            )));
        }
        StatusCode::FORBIDDEN | StatusCode::NOT_FOUND | StatusCode::GONE => {
            return Err(Failure::Refresh(anyhow!(
                "HTTP {status}, requesting a fresh URL"
            )));
        }
        s if s.is_server_error()
            || s == StatusCode::TOO_MANY_REQUESTS
            || s == StatusCode::REQUEST_TIMEOUT =>
        {
            return Err(Failure::Transient(anyhow!("HTTP {s}")));
        }
        s => return Err(Failure::Fatal(anyhow!("HTTP {s}"))),
    };

    // Validators for If-Range: only strong ETags are kept (weak "W/" ETags are not
    // allowed in If-Range).
    if !append || (meta.etag.is_none() && meta.last_modified.is_none()) {
        meta.etag = header_string(resp.headers(), header::ETAG).filter(|e| !e.starts_with("W/"));
        meta.last_modified = header_string(resp.headers(), header::LAST_MODIFIED);
    }
    if let Err(e) = save_meta(metap, meta).await {
        return Err(Failure::Fatal(e.context("writing resume metadata")));
    }

    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .append(append)
        .truncate(!append)
        .open(part)
        .await
        .map_err(|e| Failure::Fatal(anyhow!(e).context(format!("opening {}", part.display()))))?;
    let mut writer = BufWriter::with_capacity(256 * 1024, file);

    match meta.total {
        Some(t) => {
            pb.set_style(progress_style(true));
            pb.set_length(t);
        }
        None => pb.set_style(progress_style(false)),
    }
    pb.set_position(offset);
    pb.reset_eta();

    let mut stream = resp.bytes_stream();
    let streamed: Result<(), Failure> = loop {
        match timeout(opts.stall_timeout, stream.next()).await {
            Err(_) => {
                break Err(Failure::Transient(anyhow!(
                    "no data received for {}s",
                    opts.stall_timeout.as_secs()
                )))
            }
            Ok(None) => break Ok(()),
            Ok(Some(Err(e))) => {
                break Err(Failure::Transient(anyhow!(e).context("stream interrupted")))
            }
            Ok(Some(Ok(chunk))) => {
                if let Err(e) = writer.write_all(&chunk).await {
                    break Err(Failure::Fatal(anyhow!(e).context("writing to disk")));
                }
                offset += chunk.len() as u64;
                pb.set_position(offset);
            }
        }
    };

    // Whatever happened, flush what was received so the download can resume.
    if let Err(e) = writer.flush().await {
        return Err(Failure::Fatal(anyhow!(e).context("writing to disk")));
    }
    let _ = writer.into_inner().sync_all().await;
    streamed?;

    if let Some(total) = meta.total {
        if offset < total {
            return Err(Failure::Transient(anyhow!(
                "connection closed early ({offset}/{total} bytes)"
            )));
        }
        if offset > total {
            reset(part, metap, meta).await;
            return Err(Failure::Transient(anyhow!(
                "received more bytes than announced, restarting"
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_range() {
        let mut h = header::HeaderMap::new();
        h.insert(header::CONTENT_RANGE, "bytes 100-999/1000".parse().unwrap());
        assert_eq!(parse_content_range(&h), Some((Some(100), Some(1000))));
        h.insert(header::CONTENT_RANGE, "bytes */1000".parse().unwrap());
        assert_eq!(parse_content_range(&h), Some((None, Some(1000))));
        h.insert(header::CONTENT_RANGE, "bytes 5-9/*".parse().unwrap());
        assert_eq!(parse_content_range(&h), Some((Some(5), None)));
    }

    #[test]
    fn backoff_is_capped() {
        assert_eq!(backoff(1), Duration::from_secs(1));
        assert_eq!(backoff(3), Duration::from_secs(4));
        assert_eq!(backoff(50), Duration::from_secs(60));
    }
}

#[cfg(test)]
mod live_tests {
    use super::*;
    /// TEST_URL=http://… TEST_DEST=/tmp/x cargo test -- --ignored resume_against_server
    #[tokio::test]
    #[ignore]
    async fn resume_against_server() {
        let url = std::env::var("TEST_URL").unwrap();
        let dest = PathBuf::from(std::env::var("TEST_DEST").unwrap());
        let client = Client::builder().no_proxy().build().unwrap();
        let opts = Options {
            max_retries: 5,
            stall_timeout: Duration::from_secs(5),
        };
        let size = download_with_resume(&client, &dest, &opts, || {
            let u = url.clone();
            async move { Ok(u) }
        })
        .await
        .unwrap();
        println!("size={size}");
    }
}
