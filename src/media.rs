use anyhow::{anyhow, bail, Context, Result};
use base64::Engine;
use serde_json::Value;
use std::{
    collections::VecDeque,
    path::{Path, PathBuf},
    process::Stdio,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, BufReader},
    process::Command,
};

use crate::config::Config;

const PROGRESS_PREFIX: &str = "[forager]";
const MAX_IMAGES: usize = 4;
const MAX_IMAGE_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, Default, Clone, PartialEq)]
pub struct MediaInfo {
    pub title: Option<String>,
    pub description: Option<String>,
    pub thumbnail: Option<String>,
    pub uploader: Option<String>,
    pub platform: Option<String>,
    pub duration: Option<f64>,
    pub images: Vec<String>,
    pub has_audio: bool,
}

fn str_field(v: &Value, keys: &[&str]) -> Option<String> {
    keys.iter()
        .filter_map(|k| v.get(*k).and_then(Value::as_str))
        .map(str::trim)
        .find(|s| !s.is_empty())
        .map(str::to_string)
}

fn platform_name(key: &str) -> String {
    match key.to_ascii_lowercase().as_str() {
        "youtube" | "youtubetab" => "YouTube".into(),
        "tiktok" => "TikTok".into(),
        "instagram" | "instagramstory" => "Instagram".into(),
        "facebook" => "Facebook".into(),
        "pinterest" => "Pinterest".into(),
        "twitter" => "X".into(),
        _ => key.to_string(),
    }
}

pub fn parse_ytdlp_info(v: &Value) -> MediaInfo {
    let first = v
        .get("entries")
        .and_then(Value::as_array)
        .and_then(|e| e.first())
        .unwrap_or(v);
    let pick = |keys: &[&str]| str_field(v, keys).or_else(|| str_field(first, keys));
    let has_audio = first.get("acodec").and_then(Value::as_str) != Some("none");
    MediaInfo {
        title: pick(&["title", "fulltitle"]),
        description: pick(&["description"]),
        thumbnail: pick(&["thumbnail"]),
        uploader: pick(&["uploader", "channel", "uploader_id"]),
        platform: pick(&["extractor_key", "extractor"]).map(|p| platform_name(&p)),
        duration: first
            .get("duration")
            .or_else(|| v.get("duration"))
            .and_then(Value::as_f64),
        images: Vec::new(),
        has_audio,
    }
}

pub fn parse_gallery_dl(v: &Value) -> MediaInfo {
    let mut info = MediaInfo::default();
    for msg in v.as_array().into_iter().flatten() {
        let Some(kind) = msg.get(0).and_then(Value::as_i64) else {
            continue;
        };
        let meta = match kind {
            2 => msg.get(1),
            3 => msg.get(2),
            _ => None,
        };
        if let Some(meta) = meta {
            info.description = info
                .description
                .or_else(|| str_field(meta, &["description", "content", "caption"]));
            info.uploader = info
                .uploader
                .or_else(|| str_field(meta, &["username", "author", "uploader"]));
            info.title = info
                .title
                .or_else(|| str_field(meta, &["title", "fullname", "username"]));
            info.platform = info
                .platform
                .or_else(|| str_field(meta, &["category"]).map(|p| platform_name(&p)));
        }
        if kind == 3 {
            let ext = msg
                .get(2)
                .and_then(|m| m.get("extension"))
                .and_then(Value::as_str)
                .unwrap_or("jpg");
            if let Some(url) = msg.get(1).and_then(Value::as_str) {
                if url.starts_with("http")
                    && matches!(ext, "jpg" | "jpeg" | "png" | "webp" | "heic")
                    && info.images.len() < MAX_IMAGES
                {
                    info.images.push(url.to_string());
                }
            }
        }
    }
    info.thumbnail = info.images.first().cloned();
    info
}

/// Bytes downloaded so far and best-known total, from our yt-dlp progress template.
pub fn parse_progress(line: &str) -> Option<(f64, Option<f64>)> {
    let rest = line.trim().strip_prefix(PROGRESS_PREFIX)?;
    let mut parts = rest.split_whitespace().map(|p| p.parse::<f64>().ok());
    let downloaded = parts.next()??;
    let total = parts.next().flatten();
    let estimate = parts.next().flatten();
    Some((downloaded, total.or(estimate).filter(|t| *t > 0.0)))
}

struct Tail(VecDeque<String>);

impl Tail {
    fn push(&mut self, line: String) {
        if self.0.len() == 12 {
            self.0.pop_front();
        }
        self.0.push_back(line);
    }

    fn text(&self) -> String {
        self.0.iter().cloned().collect::<Vec<_>>().join("\n")
    }
}

async fn collect_tail(stream: impl AsyncRead + Unpin) -> String {
    let mut tail = Tail(VecDeque::new());
    let mut lines = BufReader::new(stream).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        if !line.trim().is_empty() {
            tail.push(line);
        }
    }
    tail.text()
}

fn command_error(program: &str, status: std::process::ExitStatus, stderr: &str) -> anyhow::Error {
    let detail = stderr
        .lines()
        .rev()
        .find(|l| l.contains("ERROR"))
        .unwrap_or_else(|| stderr.lines().last().unwrap_or(""));
    anyhow!("{program} exited with {status}: {}", detail.trim())
}

async fn run_json(mut cmd: Command, program: &str) -> Result<Value> {
    let output = cmd
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output()
        .await
        .with_context(|| format!("failed to start {program}"))?;
    if !output.status.success() {
        return Err(command_error(
            program,
            output.status,
            &String::from_utf8_lossy(&output.stderr),
        ));
    }
    serde_json::from_slice(&output.stdout)
        .with_context(|| format!("{program} returned invalid JSON"))
}

pub struct Media<'a> {
    pub config: &'a Config,
    pub cookies: Option<PathBuf>,
}

impl Media<'_> {
    fn ytdlp(&self) -> Command {
        let mut cmd = Command::new(&self.config.ytdlp);
        cmd.args(["--no-warnings", "--no-progress", "--ignore-config"]);
        if let Some(c) = &self.cookies {
            cmd.arg("--cookies").arg(c);
        }
        cmd
    }

    pub async fn ytdlp_info(&self, url: &str) -> Result<MediaInfo> {
        let mut cmd = self.ytdlp();
        cmd.args(["--dump-single-json", "--no-playlist", "--", url]);
        Ok(parse_ytdlp_info(&run_json(cmd, "yt-dlp").await?))
    }

    pub async fn gallery_dl_info(&self, url: &str) -> Result<MediaInfo> {
        let mut cmd = Command::new(&self.config.gallery_dl);
        cmd.arg("--dump-json");
        if let Some(c) = &self.cookies {
            cmd.arg("--cookies").arg(c);
        }
        cmd.args(["--", url]);
        let info = parse_gallery_dl(&run_json(cmd, "gallery-dl").await?);
        if info.description.is_none() && info.images.is_empty() {
            bail!("gallery-dl found no caption or images");
        }
        Ok(info)
    }

    /// Downloads the best audio track, reporting fractional progress as it goes.
    pub async fn download_audio(
        &self,
        url: &str,
        dir: &Path,
        mut on_progress: impl FnMut(f64),
    ) -> Result<PathBuf> {
        let mut cmd = self.ytdlp();
        cmd.args([
            "--progress",
            "--newline",
            "--quiet",
            "--progress-template",
            &format!(
                "download:{PROGRESS_PREFIX} %(progress.downloaded_bytes)s \
                 %(progress.total_bytes)s %(progress.total_bytes_estimate)s"
            ),
            "--format",
            "bestaudio/best",
            "--playlist-items",
            "1",
            "--output",
        ])
        .arg(dir.join("source.%(ext)s"))
        .args(["--", url])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

        let mut child = cmd.spawn().context("failed to start yt-dlp")?;
        let stderr = tokio::spawn(collect_tail(child.stderr.take().expect("piped stderr")));
        let mut lines = BufReader::new(child.stdout.take().expect("piped stdout")).lines();
        while let Some(line) = lines.next_line().await? {
            if let Some((done, Some(total))) = parse_progress(&line) {
                on_progress((done / total).clamp(0.0, 1.0));
            }
        }
        let status = child.wait().await?;
        let stderr = stderr.await.unwrap_or_default();
        if !status.success() {
            return Err(command_error("yt-dlp", status, &stderr));
        }

        let mut entries = tokio::fs::read_dir(dir).await?;
        while let Some(entry) = entries.next_entry().await? {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with("source.") && !name.ends_with(".part") {
                return Ok(entry.path());
            }
        }
        bail!("yt-dlp finished but produced no file")
    }

    /// Re-encodes to small mono MP3 so long videos stay under transcription upload limits.
    pub async fn to_speech_mp3(&self, input: &Path, dir: &Path) -> Result<PathBuf> {
        let out = dir.join("speech.mp3");
        let output = Command::new(&self.config.ffmpeg)
            .args(["-hide_banner", "-loglevel", "error", "-y", "-i"])
            .arg(input)
            .args(["-vn", "-ac", "1", "-ar", "16000", "-b:a", "32k"])
            .arg(&out)
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .output()
            .await
            .context("failed to start ffmpeg")?;
        if !output.status.success() {
            return Err(command_error(
                "ffmpeg",
                output.status,
                &String::from_utf8_lossy(&output.stderr),
            ));
        }
        Ok(out)
    }
}

pub async fn fetch_images_as_data_urls(http: &reqwest::Client, urls: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    for url in urls.iter().take(MAX_IMAGES) {
        let Ok(resp) = http
            .get(url)
            .send()
            .await
            .and_then(|r| r.error_for_status())
        else {
            continue;
        };
        let mime = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .filter(|v| v.starts_with("image/"))
            .unwrap_or("image/jpeg")
            .to_string();
        let Ok(bytes) = resp.bytes().await else {
            continue;
        };
        if bytes.len() <= MAX_IMAGE_BYTES {
            let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
            out.push(format!("data:{mime};base64,{b64}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_progress_lines() {
        assert_eq!(
            parse_progress("[forager] 50 100 NA"),
            Some((50.0, Some(100.0)))
        );
        assert_eq!(
            parse_progress("[forager] 50 NA 200.5"),
            Some((50.0, Some(200.5)))
        );
        assert_eq!(parse_progress("[forager] 50 NA NA"), Some((50.0, None)));
        assert_eq!(parse_progress("[download] 50%"), None);
    }

    #[test]
    fn parses_ytdlp_video_and_playlist() {
        let video = parse_ytdlp_info(&json!({
            "title": "Crispy tofu",
            "description": "1 block tofu",
            "thumbnail": "https://img/t.jpg",
            "uploader": "chef",
            "extractor_key": "TikTok",
            "duration": 42.0,
            "acodec": "aac"
        }));
        assert_eq!(video.platform.as_deref(), Some("TikTok"));
        assert_eq!(video.duration, Some(42.0));
        assert!(video.has_audio);

        let carousel = parse_ytdlp_info(&json!({
            "_type": "playlist",
            "extractor_key": "Instagram",
            "entries": [{"description": "caption", "thumbnail": "https://img/1.jpg", "acodec": "none"}]
        }));
        assert_eq!(carousel.description.as_deref(), Some("caption"));
        assert_eq!(carousel.thumbnail.as_deref(), Some("https://img/1.jpg"));
        assert!(!carousel.has_audio);
    }

    #[test]
    fn parses_gallery_dl_messages() {
        let info = parse_gallery_dl(&json!([
            [2, {"category": "instagram", "username": "chef", "description": "Pasta recipe"}],
            [3, "https://cdn/1.jpg", {"extension": "jpg"}],
            [3, "https://cdn/2.mp4", {"extension": "mp4"}],
            [3, "https://cdn/3.webp", {"extension": "webp"}]
        ]));
        assert_eq!(info.description.as_deref(), Some("Pasta recipe"));
        assert_eq!(info.platform.as_deref(), Some("Instagram"));
        assert_eq!(info.images, vec!["https://cdn/1.jpg", "https://cdn/3.webp"]);
        assert_eq!(info.thumbnail.as_deref(), Some("https://cdn/1.jpg"));
    }
}
