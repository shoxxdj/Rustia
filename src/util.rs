use std::future::Future;
use std::sync::OnceLock;
use std::time::Duration;

use anyhow::Result;
use regex::Regex;

fn re(cell: &'static OnceLock<Regex>, pattern: &str) -> &'static Regex {
    cell.get_or_init(|| Regex::new(pattern).expect("valid regex"))
}

/// Safe filename from a title: HTML entities decoded, forbidden characters replaced,
/// whitespace collapsed, truncated to 200 bytes without splitting a UTF-8 character.
pub fn safe_filename(title: &str) -> String {
    static UNSAFE: OnceLock<Regex> = OnceLock::new();
    static SPACES: OnceLock<Regex> = OnceLock::new();

    let decoded = html_escape::decode_html_entities(title);
    let replaced = re(&UNSAFE, r#"[<>:"/\\|?*\x00-\x1f\x7f]"#).replace_all(&decoded, "_");
    let collapsed = re(&SPACES, r"\s+").replace_all(&replaced, " ");
    let mut name = collapsed.trim().trim_end_matches('.').to_string();

    if name.len() > 200 {
        let mut cut = 200;
        while !name.is_char_boundary(cut) {
            cut -= 1;
        }
        name.truncate(cut);
        name = name.trim_end().to_string();
    }
    if name.is_empty() || name == "." || name == ".." {
        name = "video".into();
    }
    name
}

/// `wmediaid=` or `/medias/<id>` in a URL.
pub fn video_id_from_url(url: &str) -> Option<String> {
    static WMEDIA: OnceLock<Regex> = OnceLock::new();
    static MEDIAS: OnceLock<Regex> = OnceLock::new();
    re(&WMEDIA, r"[?&]wmediaid=([a-zA-Z0-9]+)")
        .captures(url)
        .or_else(|| re(&MEDIAS, r"/medias/([a-zA-Z0-9]+)").captures(url))
        .map(|c| c[1].to_string())
}

/// `wvideo=<id>` in an HTML snippet from "Copy link".
pub fn video_id_from_html(snippet: &str) -> Option<String> {
    static WVIDEO: OnceLock<Regex> = OnceLock::new();
    re(&WVIDEO, r"wvideo=([a-zA-Z0-9]+)")
        .captures(snippet)
        .map(|c| c[1].to_string())
}

/// Share-link hash (`https://x.wistia.com/<segment>/<hash>`).
pub fn link_hash_from_url(url: &str) -> Option<String> {
    static LINK: OnceLock<Regex> = OnceLock::new();
    re(&LINK, r"wistia\.com/[^/]+/([a-zA-Z0-9]+)")
        .captures(url)
        .map(|c| c[1].to_string())
}

pub fn is_valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.chars().all(|c| c.is_ascii_alphanumeric())
}

pub fn is_channel_url(url: &str) -> bool {
    url.contains("/embed/channel/") || url.contains("wchannelid=")
}

/// Retries a small request (metadata, HTML page) with increasing delay.
pub async fn with_retry<T, F, Fut>(attempts: u32, mut f: F) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T>>,
{
    let mut n = 0;
    loop {
        n += 1;
        match f().await {
            Ok(v) => return Ok(v),
            Err(e) if n >= attempts => return Err(e),
            Err(_) => tokio::time::sleep(Duration::from_secs(1 << n.min(5))).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filenames() {
        assert_eq!(safe_filename("A &amp; B: <test>?"), "A & B_ _test__");
        assert_eq!(safe_filename("  a   b  "), "a b");
        let long = "é".repeat(150); // 300 bytes
        let out = safe_filename(&long);
        assert!(out.len() <= 200 && out.chars().all(|c| c == 'é'));
        assert_eq!(safe_filename("///"), "___");
        assert_eq!(safe_filename(""), "video");
    }

    #[test]
    fn ids() {
        assert_eq!(
            video_id_from_url(
                "https://fast.wistia.com/embed/channel/m9k?wchannelid=m9k&wmediaid=p5v8q3n7rb"
            )
            .as_deref(),
            Some("p5v8q3n7rb")
        );
        assert_eq!(
            video_id_from_url("https://example.wistia.com/medias/abc123").as_deref(),
            Some("abc123")
        );
        assert_eq!(
            video_id_from_html(r#"<a href="x?wvideo=zz9">"#).as_deref(),
            Some("zz9")
        );
        assert!(is_valid_id("abc123"));
        assert!(!is_valid_id("../etc"));
    }
}
