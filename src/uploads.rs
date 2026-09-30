//! Files uploaded for import (photos, screenshots, recipe text, Mealie exports,
//! videos): classification and on-disk storage under `UPLOAD_DIR/<job id>/`.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use std::path::{Path, PathBuf};

use crate::urls;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Image,
    Text,
    Zip,
    Media,
}

/// What an uploaded part turned out to be.
#[derive(Debug, PartialEq, Eq)]
pub enum Classified {
    File(Kind),
    /// A shared link (a small text file or text field holding a URL).
    Link(String),
}

fn extension(name: &str) -> String {
    Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
}

fn sniff(bytes: &[u8]) -> Option<Kind> {
    let starts = |magic: &[u8]| bytes.starts_with(magic);
    if starts(&[0xFF, 0xD8, 0xFF])
        || starts(b"\x89PNG")
        || starts(b"GIF8")
        || (starts(b"RIFF") && bytes.get(8..12) == Some(b"WEBP"))
    {
        return Some(Kind::Image);
    }
    if starts(b"PK\x03\x04") {
        return Some(Kind::Zip);
    }
    if bytes.get(4..8) == Some(b"ftyp") {
        let brand = bytes.get(8..12).unwrap_or_default();
        let image = [
            b"heic", b"heix", b"hevc", b"heim", b"heis", b"mif1", b"msf1", b"avif",
        ];
        return Some(if image.iter().any(|b| brand == *b) {
            Kind::Image
        } else {
            Kind::Media
        });
    }
    if starts(b"ID3") || starts(b"OggS") || starts(b"\x1A\x45\xDF\xA3") {
        return Some(Kind::Media);
    }
    if starts(b"RIFF") && bytes.get(8..12) == Some(b"WAVE") {
        return Some(Kind::Media);
    }
    None
}

fn by_type(content_type: &str) -> Option<Kind> {
    let t = content_type.split(';').next().unwrap_or_default().trim();
    match t {
        _ if t.starts_with("image/") => Some(Kind::Image),
        _ if t.starts_with("video/") || t.starts_with("audio/") => Some(Kind::Media),
        "application/zip" | "application/x-zip-compressed" => Some(Kind::Zip),
        _ if t.starts_with("text/") || t == "application/json" => Some(Kind::Text),
        _ => None,
    }
}

fn by_extension(name: &str) -> Option<Kind> {
    match extension(name).as_str() {
        "jpg" | "jpeg" | "png" | "gif" | "webp" | "heic" | "heif" | "avif" => Some(Kind::Image),
        "mp4" | "mov" | "m4v" | "webm" | "mkv" | "mp3" | "m4a" | "wav" | "aac" | "ogg" | "opus"
        | "flac" => Some(Kind::Media),
        "zip" => Some(Kind::Zip),
        "txt" | "text" | "md" | "html" | "htm" | "json" => Some(Kind::Text),
        _ => None,
    }
}

/// Classifies an upload by its content first, then its declared type and name.
/// Short text holding a link counts as that link. `None` means unsupported.
pub fn classify(name: &str, content_type: Option<&str>, bytes: &[u8]) -> Option<Classified> {
    if bytes.starts_with(b"%PDF") {
        return None;
    }
    let kind = sniff(bytes)
        .or_else(|| content_type.and_then(by_type))
        .or_else(|| by_extension(name))
        .or_else(|| std::str::from_utf8(bytes).ok().map(|_| Kind::Text))?;
    if kind == Kind::Text {
        let text = std::str::from_utf8(bytes).ok()?;
        if text.trim().is_empty() {
            return None;
        }
        if text.len() <= 1000 {
            if let Some(url) = urls::extract(text) {
                return Some(Classified::Link(url));
            }
        }
    }
    Some(Classified::File(kind))
}

/// The job media kind for a set of uploads, or why they can't go together.
pub fn media_kind(kinds: &[Kind]) -> Result<&'static str> {
    let count = |k: Kind| kinds.iter().filter(|x| **x == k).count();
    match (count(Kind::Zip), count(Kind::Media)) {
        (0, 0) if count(Kind::Image) > 0 => Ok("images"),
        (0, 0) if count(Kind::Text) > 0 => Ok("text"),
        (0, 0) => bail!("nothing to import"),
        (1, 0) if kinds.len() == 1 => Ok("zip"),
        (0, 1) if kinds.len() == 1 => Ok("video"),
        _ => bail!("upload a Mealie .zip or a video/audio file on its own"),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Stored {
    /// File name inside the job's upload directory.
    pub file: String,
    /// Name the client gave it.
    pub name: String,
    pub kind: Kind,
}

pub struct Upload {
    pub name: String,
    pub kind: Kind,
    pub bytes: Vec<u8>,
}

fn safe_name(name: &str) -> String {
    let base = Path::new(name)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("upload");
    let cleaned: String = base
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let cleaned = cleaned.trim_start_matches('.');
    if cleaned.is_empty() {
        "upload".into()
    } else {
        cleaned.chars().take(80).collect()
    }
}

pub fn job_dir(root: &Path, id: i64) -> PathBuf {
    root.join(id.to_string())
}

/// Writes the files for a job and a manifest describing them.
pub async fn store(root: &Path, id: i64, uploads: &[Upload]) -> Result<()> {
    let dir = job_dir(root, id);
    tokio::fs::create_dir_all(&dir)
        .await
        .with_context(|| format!("could not create {}", dir.display()))?;
    let mut manifest = Vec::new();
    for (i, upload) in uploads.iter().enumerate() {
        let file = format!("{i}-{}", safe_name(&upload.name));
        tokio::fs::write(dir.join(&file), &upload.bytes).await?;
        manifest.push(Stored {
            file,
            name: upload.name.clone(),
            kind: upload.kind,
        });
    }
    tokio::fs::write(dir.join("manifest.json"), serde_json::to_vec(&manifest)?).await?;
    Ok(())
}

pub async fn load(root: &Path, id: i64) -> Result<Vec<(Stored, Vec<u8>)>> {
    let dir = job_dir(root, id);
    let manifest = tokio::fs::read(dir.join("manifest.json"))
        .await
        .context("the uploaded files are gone; upload them again")?;
    let manifest: Vec<Stored> = serde_json::from_slice(&manifest)?;
    let mut out = Vec::new();
    for stored in manifest {
        let bytes = tokio::fs::read(dir.join(&stored.file)).await?;
        out.push((stored, bytes));
    }
    Ok(out)
}

/// Removes upload directories whose job no longer exists.
pub async fn prune(db: &SqlitePool, root: &Path) -> Result<usize> {
    let Ok(mut entries) = tokio::fs::read_dir(root).await else {
        return Ok(0);
    };
    let ids: Vec<i64> = sqlx::query_scalar("SELECT id FROM jobs WHERE source = 'file'")
        .fetch_all(db)
        .await?;
    let mut removed = 0;
    while let Some(entry) = entries.next_entry().await? {
        // An upload still being committed isn't visible in `jobs` yet.
        let recent = entry
            .metadata()
            .await
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.elapsed().ok())
            .is_none_or(|age| age.as_secs() < 600);
        if recent {
            continue;
        }
        let Some(id) = entry
            .file_name()
            .to_str()
            .and_then(|n| n.parse::<i64>().ok())
        else {
            continue;
        };
        if !ids.contains(&id) {
            tokio::fs::remove_dir_all(entry.path()).await?;
            removed += 1;
        }
    }
    Ok(removed)
}

pub fn mime(stored: &Stored) -> &'static str {
    match extension(&stored.name).as_str() {
        "png" => "image/png",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "heic" | "heif" => "image/heic",
        "avif" => "image/avif",
        _ if stored.kind == Kind::Image => "image/jpeg",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_by_content_then_type_then_name() {
        let jpeg = [0xFF, 0xD8, 0xFF, 0xE0, 0, 0];
        assert_eq!(
            classify("x.bin", Some("application/octet-stream"), &jpeg),
            Some(Classified::File(Kind::Image))
        );
        let heic = b"\0\0\0\x18ftypheic\0\0\0\0";
        assert_eq!(
            classify("IMG_1.HEIC", None, heic),
            Some(Classified::File(Kind::Image))
        );
        let mov = b"\0\0\0\x14ftypqt  \0\0\0\0";
        assert_eq!(
            classify("clip.mov", None, mov),
            Some(Classified::File(Kind::Media))
        );
        assert_eq!(
            classify("a.zip", None, b"PK\x03\x04rest"),
            Some(Classified::File(Kind::Zip))
        );
        assert_eq!(
            classify("r.pdf", Some("application/pdf"), b"%PDF-1.7"),
            None
        );
        assert_eq!(
            classify("recipe.txt", Some("text/plain"), b"2 cups flour\nMix well."),
            Some(Classified::File(Kind::Text))
        );
        assert_eq!(
            classify(
                "",
                None,
                b"Look at this https://www.tiktok.com/@a/video/1?_r=1"
            ),
            Some(Classified::Link("https://www.tiktok.com/@a/video/1".into()))
        );
        assert_eq!(classify("x", None, b"   "), None);
        assert_eq!(classify("x", None, &[0xFF, 0xFE, 0x00, 0x01]), None);
    }

    #[test]
    fn groups_kinds() {
        use Kind::*;
        assert_eq!(media_kind(&[Image, Text, Image]).unwrap(), "images");
        assert_eq!(media_kind(&[Text]).unwrap(), "text");
        assert_eq!(media_kind(&[Zip]).unwrap(), "zip");
        assert_eq!(media_kind(&[Media]).unwrap(), "video");
        assert!(media_kind(&[Media, Image]).is_err());
        assert!(media_kind(&[Zip, Zip]).is_err());
        assert!(media_kind(&[]).is_err());
    }

    #[test]
    fn sanitizes_names() {
        assert_eq!(safe_name("../../etc/passwd"), "passwd");
        assert_eq!(safe_name("My Photo (1).HEIC"), "My_Photo__1_.HEIC");
        assert_eq!(safe_name(".hidden"), "hidden");
        assert_eq!(safe_name(""), "upload");
    }

    #[tokio::test]
    async fn stores_loads_and_prunes() {
        let db = crate::db::connect_memory().await.unwrap();
        let root = tempfile::tempdir().unwrap();
        let keep = crate::db::insert_job(&db, "upload:a.jpg", "file", &[], None)
            .await
            .unwrap();
        let upload = Upload {
            name: "a.jpg".into(),
            kind: Kind::Image,
            bytes: vec![1, 2, 3],
        };
        store(root.path(), keep, std::slice::from_ref(&upload))
            .await
            .unwrap();
        store(root.path(), 999, &[upload]).await.unwrap();
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
        for id in [keep, 999] {
            std::fs::File::open(job_dir(root.path(), id))
                .unwrap()
                .set_modified(old)
                .unwrap();
        }
        let loaded = load(root.path(), keep).await.unwrap();
        assert_eq!(loaded[0].0.name, "a.jpg");
        assert_eq!(loaded[0].1, vec![1, 2, 3]);
        assert_eq!(prune(&db, root.path()).await.unwrap(), 1);
        assert!(load(root.path(), 999).await.is_err());
        assert!(load(root.path(), keep).await.is_ok());
    }
}
