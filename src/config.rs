use anyhow::{Context, Result};
use std::{env, net::SocketAddr, path::PathBuf};

#[derive(Debug, Clone)]
pub struct Config {
    pub listen: SocketAddr,
    pub database_path: PathBuf,
    pub upload_dir: PathBuf,
    pub max_upload_bytes: usize,
    pub work_dir: PathBuf,
    pub workers: usize,
    pub max_duration_secs: f64,

    pub openai_url: String,
    pub openai_api_key: String,
    pub transcription_model: String,
    pub text_model: String,
    pub extra_prompt: Option<String>,

    pub cleanup: bool,
    pub clean_model: String,
    pub clean_tag: String,

    pub mealie_url: String,
    pub mealie_public_url: String,
    pub mealie_api_key: String,
    pub mealie_group: String,

    pub ytdlp: String,
    pub ffmpeg: String,
    pub gallery_dl: String,
    pub cookies_file: Option<PathBuf>,
}

fn var(name: &str) -> Option<String> {
    env::var(name)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

fn required(name: &str) -> Result<String> {
    var(name).with_context(|| format!("{name} must be set"))
}

fn url_var(name: &str) -> Option<String> {
    var(name).map(|v| v.trim_end_matches('/').to_string())
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let mealie_url = url_var("MEALIE_URL").context("MEALIE_URL must be set")?;
        let database_path: PathBuf = var("DATABASE_PATH")
            .unwrap_or_else(|| "mealie-forager.db".into())
            .into();
        let upload_dir = var("UPLOAD_DIR").map_or_else(
            || {
                database_path
                    .parent()
                    .unwrap_or(std::path::Path::new(""))
                    .join("uploads")
            },
            PathBuf::from,
        );
        Ok(Self {
            listen: var("LISTEN_ADDR")
                .unwrap_or_else(|| "127.0.0.1:3000".into())
                .parse()
                .context("LISTEN_ADDR is not a socket address")?,
            database_path,
            upload_dir,
            max_upload_bytes: var("MAX_UPLOAD_MB")
                .and_then(|v| v.parse::<usize>().ok())
                .unwrap_or(100)
                .max(1)
                * 1024
                * 1024,
            work_dir: var("WORK_DIR").map_or_else(env::temp_dir, PathBuf::from),
            workers: var("WORKERS")
                .and_then(|v| v.parse().ok())
                .unwrap_or(2)
                .max(1),
            max_duration_secs: var("MAX_DURATION_SECS")
                .and_then(|v| v.parse().ok())
                .unwrap_or(1800.0),

            openai_url: url_var("OPENAI_URL").unwrap_or_else(|| "https://api.openai.com/v1".into()),
            openai_api_key: required("OPENAI_API_KEY")?,
            transcription_model: var("TRANSCRIPTION_MODEL").unwrap_or_else(|| "whisper-1".into()),
            text_model: var("TEXT_MODEL").unwrap_or_else(|| "gpt-5-mini".into()),
            extra_prompt: var("EXTRA_PROMPT"),

            cleanup: var("CLEANUP").is_none_or(|v| {
                !matches!(
                    v.to_ascii_lowercase().as_str(),
                    "0" | "false" | "no" | "off"
                )
            }),
            clean_model: var("CLEAN_MODEL")
                .or_else(|| var("TEXT_MODEL"))
                .unwrap_or_else(|| "gpt-5-mini".into()),
            clean_tag: var("CLEAN_TAG").unwrap_or_else(|| "Imported Clean".into()),

            mealie_public_url: url_var("MEALIE_PUBLIC_URL").unwrap_or_else(|| mealie_url.clone()),
            mealie_url,
            mealie_api_key: required("MEALIE_API_KEY")?.replace('\n', ""),
            mealie_group: var("MEALIE_GROUP_NAME").unwrap_or_else(|| "home".into()),

            ytdlp: var("YTDLP_PATH").unwrap_or_else(|| "yt-dlp".into()),
            ffmpeg: var("FFMPEG_PATH").unwrap_or_else(|| "ffmpeg".into()),
            gallery_dl: var("GALLERY_DL_PATH").unwrap_or_else(|| "gallery-dl".into()),
            cookies_file: var("COOKIES_FILE").map(PathBuf::from),
        })
    }

    pub fn mealie_image_link(&self, recipe_id: &str) -> String {
        format!(
            "{}/api/media/recipes/{recipe_id}/images/min-original.webp",
            self.mealie_public_url
        )
    }

    /// The recipe slug if `url` is a recipe page on this Mealie
    /// (`/g/<group>/r/<slug>`, or `/recipe/<slug>` on older versions).
    pub fn mealie_slug_from_url(&self, url: &str) -> Option<String> {
        let parsed = url::Url::parse(url).ok()?;
        let ours = [&self.mealie_public_url, &self.mealie_url]
            .iter()
            .filter_map(|u| url::Url::parse(u).ok())
            .any(|u| u.host_str() == parsed.host_str() && u.port() == parsed.port());
        if !ours {
            return None;
        }
        let segments: Vec<&str> = parsed.path_segments()?.filter(|s| !s.is_empty()).collect();
        let at = segments.iter().position(|s| *s == "r" || *s == "recipe")?;
        segments.get(at + 1).map(|s| s.to_string())
    }

    pub fn mealie_recipe_link(&self, slug: &str) -> String {
        format!(
            "{}/g/{}/r/{}",
            self.mealie_public_url, self.mealie_group, slug
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> Config {
        Config {
            listen: "127.0.0.1:0".parse().unwrap(),
            database_path: "x.db".into(),
            upload_dir: "uploads".into(),
            max_upload_bytes: 1,
            work_dir: ".".into(),
            workers: 1,
            max_duration_secs: 1.0,
            openai_url: String::new(),
            openai_api_key: String::new(),
            transcription_model: String::new(),
            text_model: String::new(),
            extra_prompt: None,
            cleanup: true,
            clean_model: String::new(),
            clean_tag: String::new(),
            mealie_url: "http://127.0.0.1:9925".into(),
            mealie_public_url: "https://mealie.example.com".into(),
            mealie_api_key: String::new(),
            mealie_group: "home".into(),
            ytdlp: String::new(),
            ffmpeg: String::new(),
            gallery_dl: String::new(),
            cookies_file: None,
        }
    }

    #[test]
    fn recognises_mealie_recipe_links() {
        let c = config();
        assert_eq!(
            c.mealie_slug_from_url("https://mealie.example.com/g/home/r/garlic-noodles?x=1"),
            Some("garlic-noodles".into())
        );
        assert_eq!(
            c.mealie_slug_from_url("https://mealie.example.com/recipe/soup"),
            Some("soup".into())
        );
        assert_eq!(
            c.mealie_slug_from_url("http://127.0.0.1:9925/g/home/r/pie"),
            Some("pie".into())
        );
        assert_eq!(
            c.mealie_slug_from_url("https://mealie.example.com/g/home"),
            None
        );
        assert_eq!(c.mealie_slug_from_url("https://other.com/g/home/r/x"), None);
    }
}
