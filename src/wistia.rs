use anyhow::{anyhow, bail, Context, Result};
use base64::Engine;
use regex::Regex;
use reqwest::{header, Client};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::util::{is_valid_id, with_retry};

const MEDIA_JSON: &str = "https://fast.wistia.com/embed/medias";

pub struct Video {
    pub id: String,
    pub title: String,
    pub section: String,
}

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
pub struct ChannelData {
    pub hashed_id: String,
    pub series: Vec<Series>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
pub struct Series {
    pub sections: Vec<Section>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
pub struct Section {
    pub name: String,
    pub episodes: Vec<Episode>,
}

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
pub struct Episode {
    pub hashed_id: String,
    pub name: String,
}

pub async fn media_json(client: &Client, id: &str) -> Result<Value> {
    if !is_valid_id(id) {
        bail!("invalid video ID: {id:?}");
    }
    let url = format!("{MEDIA_JSON}/{id}.json");
    let v: Value = client
        .get(&url)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await
        .context("unreadable media JSON")?;
    if let Some(err) = v.get("error") {
        bail!("Wistia returned an error: {err}");
    }
    Ok(v)
}

pub async fn video_title(client: &Client, id: &str) -> Option<String> {
    let v = with_retry(3, || media_json(client, id)).await.ok()?;
    v.pointer("/media/name")?.as_str().map(str::to_owned)
}

/// Requested quality.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Quality {
    /// Source file uploaded to Wistia (largest).
    Original,
    /// Best MP4 re-encoded by Wistia (never the original).
    Best,
    /// Highest MP4 whose resolution (short side, e.g. 1080 for 1920x1080) does not exceed
    /// the value; otherwise the smallest available.
    Max(u32),
}

impl std::fmt::Display for Quality {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Quality::Original => write!(f, "original"),
            Quality::Best => write!(f, "best MP4"),
            Quality::Max(n) => write!(f, "{n}p max"),
        }
    }
}

impl std::str::FromStr for Quality {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "original" | "orig" | "source" => Ok(Quality::Original),
            "best" | "max" => Ok(Quality::Best),
            other => other
                .trim_end_matches('p')
                .parse::<u32>()
                .ok()
                .filter(|n| *n > 0)
                .map(Quality::Max)
                .ok_or_else(|| {
                    format!("invalid quality \"{s}\" (e.g. original, best, 1080, 720p)")
                }),
        }
    }
}

/// A video rendition offered by Wistia.
#[derive(Clone, Debug)]
pub struct Asset {
    pub kind: String,
    pub url: String,
    pub width: u32,
    pub height: u32,
    pub bitrate: u64,
    pub size: Option<u64>,
}

impl Asset {
    pub fn is_original(&self) -> bool {
        self.kind == "original"
    }
    /// Resolution "in p": the short side (also handles vertical videos).
    pub fn resolution(&self) -> u32 {
        match (self.width, self.height) {
            (0, h) | (h, 0) => h,
            (w, h) => w.min(h),
        }
    }
    pub fn describe(&self) -> String {
        let size = self
            .size
            .map(|s| format!("{:.1} MiB", s as f64 / 1_048_576.0))
            .unwrap_or_else(|| "size unknown".into());
        format!(
            "{} — {}x{} ({}p), {} kb/s, {}",
            self.kind,
            self.width,
            self.height,
            self.resolution(),
            self.bitrate,
            size
        )
    }
}

/// Downloadable video renditions (original + MP4), excluding HLS, images, storyboards.
pub fn video_assets(v: &Value) -> Vec<Asset> {
    let Some(assets) = v.pointer("/media/assets").and_then(Value::as_array) else {
        return Vec::new();
    };
    let s = |a: &Value, k: &str| a.get(k).and_then(Value::as_str).unwrap_or("").to_owned();
    let n = |a: &Value, k: &str| a.get(k).and_then(Value::as_f64).unwrap_or(0.0);

    assets
        .iter()
        .filter_map(|a| {
            let kind = s(a, "type");
            let url = s(a, "url");
            if url.is_empty() {
                return None;
            }
            let is_mp4 = s(a, "container") == "mp4" || s(a, "ext") == "mp4" || kind.contains("mp4");
            let excluded = ["hls", "still", "storyboard", "image", "audio"]
                .iter()
                .any(|x| kind.contains(x));
            if kind != "original" && (!is_mp4 || excluded) {
                return None;
            }
            Some(Asset {
                kind,
                url,
                width: n(a, "width") as u32,
                height: n(a, "height") as u32,
                bitrate: n(a, "bitrate") as u64,
                size: a.get("size").and_then(Value::as_u64),
            })
        })
        .collect()
}

pub fn select_asset(assets: &[Asset], quality: Quality) -> Option<Asset> {
    let key = |a: &&Asset| (a.resolution(), a.bitrate, a.size.unwrap_or(0));
    let encoded = || assets.iter().filter(|a| !a.is_original());

    let chosen = match quality {
        Quality::Original => assets
            .iter()
            .filter(|a| a.is_original())
            .max_by_key(key)
            .or_else(|| encoded().max_by_key(key)),
        Quality::Best => encoded().max_by_key(key),
        Quality::Max(limit) => encoded()
            .filter(|a| a.resolution() <= limit)
            .max_by_key(key)
            .or_else(|| encoded().min_by_key(key)),
    };
    // Last resort: if only the original exists, take it.
    chosen.or_else(|| assets.iter().max_by_key(key)).cloned()
}

/// Resolves the rendition to download (called again when a URL expires).
pub async fn resolve_asset(client: &Client, id: &str, quality: Quality) -> Result<Asset> {
    let v = with_retry(4, || media_json(client, id)).await?;
    select_asset(&video_assets(&v), quality)
        .ok_or_else(|| anyhow!("no downloadable video file found"))
}

pub async fn list_assets(client: &Client, id: &str) -> Result<Vec<Asset>> {
    let v = with_retry(4, || media_json(client, id)).await?;
    let mut assets = video_assets(&v);
    assets.sort_by_key(|a| (a.is_original(), a.resolution(), a.bitrate));
    Ok(assets)
}

pub async fn fetch_channel(client: &Client, page_url: &str) -> Result<ChannelData> {
    let html = with_retry(4, || async {
        Ok(client
            .get(page_url)
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?)
    })
    .await
    .context("fetching channel page")?;
    extract_channel_data(&html)
}

/// Extracts `window['wchanneljsonp-…'] = JSON.parse(decodeURIComponent(atob("…")))`.
pub fn extract_channel_data(html: &str) -> Result<ChannelData> {
    let re = Regex::new(
        r#"window\['wchanneljsonp-[^']+'\]\s*=\s*JSON\.parse\(decodeURIComponent\(atob\("([^"]+)"\)\)\)"#,
    )?;
    let encoded = re
        .captures(html)
        .map(|c| c[1].to_string())
        .ok_or_else(|| anyhow!("channel data not found in page"))?;

    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded.trim())
        .context("base64 decode failed")?;
    // decodeURIComponent does not turn "+" into a space; neither does percent_decode.
    let raw = String::from_utf8(bytes).context("channel data is not UTF-8")?;
    let decoded = percent_encoding::percent_decode_str(&raw)
        .decode_utf8()
        .context("URL decode failed")?;
    serde_json::from_str(&decoded).context("unreadable channel JSON")
}

pub fn channel_videos(data: &ChannelData) -> Vec<Video> {
    data.series
        .iter()
        .flat_map(|s| &s.sections)
        .flat_map(|sec| {
            sec.episodes.iter().map(move |ep| Video {
                id: ep.hashed_id.clone(),
                title: ep.name.clone(),
                section: sec.name.clone(),
            })
        })
        .filter(|v| is_valid_id(&v.id))
        .collect()
}

/// Resolves a Wistia share link (`https://<account>.wistia.com/<x>/<hash>`) through the
/// anonymous GraphQL API used by the page.
pub async fn resolve_link_graphql(
    client: &Client,
    page_url: &str,
    link_hash: &str,
) -> Result<String> {
    let parsed = reqwest::Url::parse(page_url).context("invalid URL")?;
    let host = parsed
        .host_str()
        .ok_or_else(|| anyhow!("URL has no host"))?;
    let origin = format!("{}://{}", parsed.scheme(), host);
    let endpoint = format!("{origin}/graphql?op=AudienceLink");

    let body = json!({
        "operationName": "AudienceLink",
        "variables": { "hashedId": link_hash },
        "query": "query AudienceLink($hashedId: HashedId!) { audienceLink(hashedId: $hashedId) { id status media { id ... on AnonymousMedia { hashedId name } __typename } __typename } }"
    });

    let resp: Value = with_retry(3, || async {
        let r = client
            .post(&endpoint)
            .header(header::ACCEPT, "*/*")
            .header("x-wistia-gql-schema", "AnonymousSchema")
            .header(header::ORIGIN, &origin)
            .header(header::REFERER, page_url)
            .json(&body)
            .send()
            .await?;
        let status = r.status();
        if !status.is_success() {
            let text = r.text().await.unwrap_or_default();
            bail!(
                "GraphQL HTTP {status}: {}",
                text.chars().take(300).collect::<String>()
            );
        }
        Ok(r.json().await?)
    })
    .await?;

    resp.pointer("/data/audienceLink/media/hashedId")
        .and_then(Value::as_str)
        .filter(|id| is_valid_id(id))
        .map(str::to_owned)
        .ok_or_else(|| anyhow!("no video ID in GraphQL response"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Vec<Asset> {
        video_assets(&json!({"media": {"assets": [
            {"type": "original", "width": 3840, "height": 2160, "bitrate": 40000, "size": 4_000_000_000u64, "url": "https://a/orig.bin"},
            {"type": "iphone_video", "container": "mp4", "width": 640, "height": 360, "bitrate": 600, "url": "https://a/360.bin"},
            {"type": "md_mp4_video", "container": "mp4", "width": 1280, "height": 720, "bitrate": 2000, "url": "https://a/720.bin"},
            {"type": "hd_mp4_video", "container": "mp4", "width": 1920, "height": 1080, "bitrate": 5000, "url": "https://a/1080.bin"},
            {"type": "hls_video", "width": 3840, "height": 2160, "bitrate": 9000, "url": "https://a/x.m3u8"},
            {"type": "still_image", "width": 1920, "height": 1080, "url": "https://a/s.jpg"},
        ]}}))
    }

    #[test]
    fn quality_selection() {
        let a = sample();
        let url = |q| select_asset(&a, q).unwrap().url;
        assert_eq!(a.len(), 4);
        assert_eq!(url(Quality::Original), "https://a/orig.bin");
        assert_eq!(url(Quality::Best), "https://a/1080.bin");
        assert_eq!(url(Quality::Max(1080)), "https://a/1080.bin");
        assert_eq!(url(Quality::Max(900)), "https://a/720.bin");
        assert_eq!(url(Quality::Max(100)), "https://a/360.bin");
        assert_eq!(url(Quality::Max(4000)), "https://a/1080.bin"); // never the original
    }

    #[test]
    fn quality_parsing() {
        assert_eq!("1080p".parse::<Quality>(), Ok(Quality::Max(1080)));
        assert_eq!("720".parse::<Quality>(), Ok(Quality::Max(720)));
        assert_eq!("Original".parse::<Quality>(), Ok(Quality::Original));
        assert_eq!("best".parse::<Quality>(), Ok(Quality::Best));
        assert!("hd".parse::<Quality>().is_err());
    }

    #[test]
    fn vertical_video_resolution() {
        let a = Asset {
            kind: "hd_mp4_video".into(),
            url: String::new(),
            width: 1080,
            height: 1920,
            bitrate: 0,
            size: None,
        };
        assert_eq!(a.resolution(), 1080);
    }

    #[test]
    fn channel_extraction() {
        let payload = r#"{"hashedId":"chan1","series":[{"sections":[{"name":"Intro","episodes":[{"hashedId":"vid1","name":"Hello%20world"}]}]}]}"#;
        let encoded_uri: String = payload.replace(' ', "%20");
        let b64 = base64::engine::general_purpose::STANDARD.encode(encoded_uri);
        let html = format!(
            r#"<script>window['wchanneljsonp-chan1'] = JSON.parse(decodeURIComponent(atob("{b64}")));</script>"#
        );
        let data = extract_channel_data(&html).unwrap();
        assert_eq!(data.hashed_id, "chan1");
        let vids = channel_videos(&data);
        assert_eq!(vids.len(), 1);
        assert_eq!(vids[0].id, "vid1");
        assert_eq!(vids[0].title, "Hello world");
        assert_eq!(vids[0].section, "Intro");
    }
}
