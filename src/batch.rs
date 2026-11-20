//! Batch download from a CSV file with header `id, folder, title`.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

use crate::util::{is_valid_id, safe_filename, video_id_from_html, video_id_from_url};

#[derive(Debug, PartialEq)]
pub struct Job {
    pub line: usize,
    pub id: String,
    pub dest: PathBuf,
}

/// Reads the CSV and builds the list of downloads.
/// - `,` or `;` delimiter, detected from the header line (Excel exports in some locales use `;`);
/// - header names are case-insensitive, trimmed, and a UTF-8 BOM is ignored;
/// - columns may be named `folder`/`title` or, for compatibility, `dossier`/`titre`;
/// - the `id` column also accepts a Wistia URL containing the ID;
/// - two rows targeting the same file get a " (2)", " (3)"… suffix.
pub fn read_jobs(path: &Path) -> Result<(Vec<Job>, Vec<String>)> {
    let raw = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let text = String::from_utf8_lossy(&raw);
    let text = text.trim_start_matches('\u{feff}');

    let header_line = text.lines().next().unwrap_or("");
    let delimiter = if header_line.matches(';').count() > header_line.matches(',').count() {
        b';'
    } else {
        b','
    };

    let mut rdr = csv::ReaderBuilder::new()
        .delimiter(delimiter)
        .flexible(true)
        .trim(csv::Trim::All)
        .from_reader(text.as_bytes());

    let headers: Vec<String> = rdr
        .headers()?
        .iter()
        .map(|h| h.trim().to_lowercase())
        .collect();
    let col = |names: &[&str]| headers.iter().position(|h| names.contains(&h.as_str()));
    let (Some(ci), Some(cf), Some(ct)) = (
        col(&["id"]),
        col(&["folder", "dossier"]),
        col(&["title", "titre"]),
    ) else {
        bail!(
            "expected CSV header: id, folder, title (found: {})",
            headers.join(", ")
        );
    };

    let mut jobs = Vec::new();
    let mut warnings = Vec::new();
    let mut used: HashSet<PathBuf> = HashSet::new();

    for (n, rec) in rdr.records().enumerate() {
        let line = n + 2; // line 1 is the header
        let rec = match rec {
            Ok(r) => r,
            Err(e) => {
                warnings.push(format!("line {line} unreadable: {e}"));
                continue;
            }
        };
        let get = |i: usize| rec.get(i).unwrap_or("").trim().to_string();
        let (raw_id, folder, title) = (get(ci), get(cf), get(ct));

        if raw_id.is_empty() && folder.is_empty() && title.is_empty() {
            continue; // blank row
        }
        let id = if is_valid_id(&raw_id) {
            raw_id.clone()
        } else if let Some(id) = video_id_from_url(&raw_id).or_else(|| video_id_from_html(&raw_id))
        {
            id
        } else {
            warnings.push(format!("line {line} skipped: invalid ID \"{raw_id}\""));
            continue;
        };

        let dir = if folder.is_empty() {
            PathBuf::from(".")
        } else {
            PathBuf::from(&folder)
        };
        let base = if title.is_empty() {
            id.clone()
        } else {
            safe_filename(&title)
        };

        let mut dest = dir.join(format!("{base}.mp4"));
        let mut k = 2;
        while !used.insert(dest.clone()) {
            dest = dir.join(format!("{base} ({k}).mp4"));
            k += 1;
        }
        jobs.push(Job { line, id, dest });
    }
    Ok((jobs, warnings))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(content: &str) -> (Vec<Job>, Vec<String>) {
        let p = std::env::temp_dir().join(format!(
            "wd-test-{}-{}.csv",
            std::process::id(),
            content.len()
        ));
        std::fs::write(&p, content).unwrap();
        let r = read_jobs(&p).unwrap();
        let _ = std::fs::remove_file(&p);
        r
    }

    #[test]
    fn comma_and_semicolon() {
        let (j, w) = parse("id, folder, title\nabc123, Course/Module 1, Intro: the basics\n");
        assert!(w.is_empty());
        assert_eq!(j[0].id, "abc123");
        assert_eq!(
            j[0].dest,
            PathBuf::from("Course/Module 1/Intro_ the basics.mp4")
        );

        // Legacy French header, semicolons, BOM, CRLF
        let (j, _) = parse("\u{feff}ID;Dossier;Titre\r\nxyz9;Vidéos;Épisode 1\r\n");
        assert_eq!(j[0].dest, PathBuf::from("Vidéos/Épisode 1.mp4"));
    }

    #[test]
    fn url_ids_duplicates_and_bad_rows() {
        let csv = "title,id,folder\n\
                   A,https://x.wistia.com/medias/abc111,d\n\
                   A,abc222,d\n\
                   B,../../etc,d\n\
                   ,,\n\
                   ,abc333,\n";
        let (j, w) = parse(csv);
        assert_eq!(j.len(), 3);
        assert_eq!(j[0].id, "abc111");
        assert_eq!(j[1].dest, PathBuf::from("d/A (2).mp4"));
        assert_eq!(j[2].dest, PathBuf::from("./abc333.mp4"));
        assert_eq!(w.len(), 1);
    }
}
