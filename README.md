# wistia-downloader

Command-line tool to download Wistia videos, whole channels, or a list of videos from a CSV file.
Downloads survive network drops: they resume where they stopped, automatically or on the next run.

## Install

Grab a prebuilt binary (Linux, Windows, macOS — x86_64) from the project's Releases page, or build it:

```bash
cargo build --release
# binary: target/release/wistia-downloader
```

## Usage

```bash
# Single video, by ID or URL
wistia-downloader -i j4n8x2m7vw
wistia-downloader -u "https://example.wistia.com/medias/h3b2k9f5xp"

# HTML snippet from "Copy link"
wistia-downloader -c '<a href="...?wvideo=j4n8x2m7vw">...</a>'

# Whole channel (saved to ./wistia_downloads by default)
wistia-downloader -u "https://fast.wistia.com/embed/channel/m9k8d7f2jq" -y

# List of videos from a CSV file
wistia-downloader --csv videos.csv -q 1080 -y
```

By default the file is named after the video title. Use `-o` to choose the output file.

### Quality

Wistia keeps the uploaded source file plus several re-encoded MP4s (typically 360p to 1080p).

| `-q` value             | Downloads                                                      |
|------------------------|----------------------------------------------------------------|
| `original` (default)   | the source file (can be several GB)                            |
| `best`                 | the best re-encoded MP4, never the original                    |
| `1080`, `720p`, `540`… | the highest MP4 not above that resolution (short side)         |

See what is available for a video:

```bash
wistia-downloader -i j4n8x2m7vw --list
```

### CSV file

Header `id, folder, title`, comma or semicolon separated:

```csv
id,folder,title
j4n8x2m7vw,Course/Module 1,Introduction
p5v8q3n7rb,Course/Module 2,The basics
```

Each video is saved as `<folder>/<title>.mp4`; missing folders are created. The `id` column also
accepts a Wistia URL. An empty title falls back to the video ID, an empty folder to the current
directory. Invalid rows are skipped with a warning.

### Resume and retries

- Data is written to `<file>.mp4.part` and renamed only once complete.
- After a drop, a server error or a stalled connection (`--stall-timeout`, 30 s by default),
  the download resumes from where it stopped.
- It gives up only after `--retries` consecutive attempts without progress (20 by default,
  `0` = never give up). A download that keeps advancing never gives up.
- If it stops anyway (Ctrl+C, crash, reboot), run the same command again: finished files are
  skipped and partial ones resume.

### All options

```
-i, --id <ID>              Wistia video ID
-u, --url <URL>            Wistia page URL (media, share link or channel)
-c, --clipboard <HTML>     HTML snippet from "Copy link"
    --csv <FILE>           CSV file (id, folder, title)
-o, --output <FILE>        Output file (single video)
-d, --dir <DIR>            Destination folder for a channel [default: wistia_downloads]
-q, --quality <QUALITY>    original | best | 1080 | 720 | … [default: original]
-l, --list                 List available renditions, don't download
-r, --retries <N>          Attempts without progress before giving up, 0 = forever [default: 20]
    --stall-timeout <SEC>  Seconds without data before reconnecting [default: 30]
-y, --yes                  Answer yes to confirmations
    --all                  Channel URL with a video: download the whole channel
    --force                Download again even if the file exists
```

## CI

`.gitlab-ci.yml` runs tests, then cross-compiles x86_64 binaries for Linux (static musl),
Windows (MinGW) and macOS (cargo-zigbuild) on standard Linux runners. Binaries are available as
job artifacts on every pipeline. Pushing a tag such as `v0.1.0` also uploads them to the Package
Registry and creates a GitLab Release.
