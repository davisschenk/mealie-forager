use anyhow::{anyhow, bail, Result};
use std::{
    sync::Mutex,
    time::{Duration, Instant},
};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use crate::{
    db::{self, Job, Stage, Status},
    mealie::{self, Mealie},
    media::{self, Media, MediaInfo},
    openai::{ExtractInput, OpenAi},
    state::AppState,
};

pub async fn run(state: AppState, index: usize) {
    info!(worker = index, "worker started");
    loop {
        let wake = state.wake.notified();
        match db::claim_next(&state.db).await {
            Ok(Some(job)) => process(&state, job).await,
            Ok(None) => {
                let _ = tokio::time::timeout(Duration::from_secs(30), wake).await;
            }
            Err(e) => {
                error!(worker = index, "failed to claim job: {e:#}");
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        }
    }
}

struct Ctx<'a> {
    state: &'a AppState,
    id: i64,
    attempt: i64,
    stage: Mutex<Stage>,
}

impl Ctx<'_> {
    fn current(&self) -> Stage {
        *self.stage.lock().unwrap()
    }

    async fn enter(&self, stage: Stage) -> Result<()> {
        let db = &self.state.db;
        db::finish_stage(db, self.id, "ok").await?;
        db::start_stage(db, self.id, self.attempt, stage).await?;
        *self.stage.lock().unwrap() = stage;
        self.state.publish_job(self.id).await;
        Ok(())
    }

    async fn log(&self, level: &str, message: impl AsRef<str>) {
        match db::log(
            &self.state.db,
            self.id,
            level,
            Some(self.current()),
            message.as_ref(),
        )
        .await
        {
            Ok(event) => self.state.publish_event(event),
            Err(e) => warn!(job = self.id, "failed to write log: {e:#}"),
        }
    }

    async fn info(&self, message: impl AsRef<str>) {
        self.log("info", message).await;
    }

    async fn warn(&self, message: impl AsRef<str>) {
        self.log("warn", message).await;
    }

    async fn progress(&self, value: Option<f64>) {
        if db::set_progress(&self.state.db, self.id, value)
            .await
            .is_ok()
        {
            self.state.publish_job(self.id).await;
        }
    }
}

async fn process(state: &AppState, job: Job) {
    let id = job.id;
    let token = CancellationToken::new();
    state.running.lock().unwrap().insert(id, token.clone());
    let ctx = Ctx {
        state,
        id,
        attempt: job.attempts,
        stage: Mutex::new(Stage::Queued),
    };
    let started = Instant::now();
    ctx.info(format!("Attempt {} started", job.attempts)).await;

    let outcome = tokio::select! {
        r = pipeline(&ctx) => Some(r),
        () = token.cancelled() => None,
    };
    state.running.lock().unwrap().remove(&id);

    let stage = ctx.current();
    let db = &state.db;
    let elapsed = format!("{:.1}s", started.elapsed().as_secs_f64());
    let result = match outcome {
        Some(Ok(slug)) => {
            let _ = db::finish_stage(db, id, "ok").await;
            ctx.info(format!("Imported into Mealie in {elapsed}")).await;
            db::finish(db, id, Status::Succeeded, Stage::Done, None, Some(&slug)).await
        }
        Some(Err(e)) => {
            let message = format!("{e:#}");
            warn!(job = id, stage = stage.as_str(), "job failed: {message}");
            let _ = db::finish_stage(db, id, "error").await;
            ctx.log("error", &message).await;
            db::finish(
                db,
                id,
                Status::Failed,
                stage,
                Some((stage.as_str(), &message)),
                None,
            )
            .await
        }
        None => {
            let _ = db::finish_stage(db, id, "cancelled").await;
            ctx.warn(format!("Cancelled after {elapsed}")).await;
            db::finish(db, id, Status::Cancelled, stage, None, None).await
        }
    };
    if let Err(e) = result {
        error!(job = id, "failed to record job outcome: {e:#}");
    }
    state.publish_job(id).await;
}

async fn pipeline(ctx: &Ctx<'_>) -> Result<String> {
    let state = ctx.state;
    let config = &state.config;
    let job = db::get_full(&state.db, ctx.id)
        .await?
        .ok_or_else(|| anyhow!("job disappeared"))?;
    let tags = &job.tags.0;

    let work = tempfile::Builder::new()
        .prefix(&format!("job-{}-", job.id))
        .tempdir_in(&config.work_dir)?;
    let cookies = match &config.cookies_file {
        Some(src) => {
            // yt-dlp rewrites its cookie jar, and the configured file is read-only.
            let dst = work.path().join("cookies.txt");
            tokio::fs::copy(src, &dst).await?;
            Some(dst)
        }
        None => None,
    };
    let media = Media { config, cookies };

    let mut transcript = job.transcript.clone();
    let mut info = MediaInfo {
        title: job.title.clone(),
        description: job.description.clone(),
        thumbnail: job.thumbnail.clone(),
        uploader: job.uploader.clone(),
        platform: job.platform.clone(),
        duration: job.duration_secs,
        images: job.images.clone().map(|i| i.0).unwrap_or_default(),
        has_audio: job.media_kind.as_deref() == Some("video"),
    };
    let have_source = transcript.is_some() || job.media_kind.as_deref() == Some("images");

    if job.recipe_json.is_none() && !have_source {
        ctx.enter(Stage::Metadata).await?;
        ctx.info(format!("Fetching post details for {}", job.url))
            .await;
        info = match media.ytdlp_info(&job.url).await {
            Ok(info) => info,
            Err(e) => {
                ctx.warn(format!(
                    "yt-dlp could not read the post ({e:#}); trying gallery-dl"
                ))
                .await;
                let mut info = media.gallery_dl_info(&job.url).await.map_err(|g| {
                    anyhow!("neither yt-dlp ({e:#}) nor gallery-dl ({g:#}) could read this post")
                })?;
                info.has_audio = false;
                info
            }
        };
        let kind = if info.has_audio {
            "video"
        } else if !info.images.is_empty() {
            "images"
        } else {
            "text"
        };
        db::set_metadata(
            &state.db,
            job.id,
            &db::Metadata {
                title: info.title.as_deref(),
                platform: info.platform.as_deref(),
                uploader: info.uploader.as_deref(),
                thumbnail: info.thumbnail.as_deref(),
                duration_secs: info.duration,
                description: info.description.as_deref(),
                media_kind: kind,
                images: &info.images,
            },
        )
        .await?;
        ctx.info(format!(
            "Found {} post{}{}",
            info.platform.as_deref().unwrap_or("a"),
            info.uploader
                .as_deref()
                .map(|u| format!(" by {u}"))
                .unwrap_or_default(),
            info.duration
                .map(|d| format!(" ({d:.0}s)"))
                .unwrap_or_default(),
        ))
        .await;
        if let Some(d) = info.duration.filter(|d| *d > config.max_duration_secs) {
            bail!(
                "video is {d:.0}s long, over the {:.0}s limit",
                config.max_duration_secs
            );
        }

        if info.has_audio {
            ctx.enter(Stage::Download).await?;
            match download(ctx, &media, &job.url, work.path()).await {
                Ok(audio) => {
                    ctx.enter(Stage::Transcribe).await?;
                    ctx.info(format!("Transcribing with {}", config.transcription_model))
                        .await;
                    let openai = OpenAi {
                        http: &state.http,
                        config,
                    };
                    let text = openai.transcribe(&audio).await?;
                    ctx.info(format!(
                        "Transcript has {} words",
                        text.split_whitespace().count()
                    ))
                    .await;
                    db::set_transcript(&state.db, job.id, &text).await?;
                    transcript = Some(text);
                }
                Err(e) if info.description.is_some() => {
                    ctx.warn(format!(
                        "Audio unavailable ({e:#}); continuing with the caption only"
                    ))
                    .await;
                }
                Err(e) => return Err(e),
            }
        }
    } else if job.recipe_json.is_none() {
        ctx.info("Reusing post details and transcript from the previous attempt")
            .await;
    }

    let recipe = match job.recipe_json.clone() {
        Some(recipe) => {
            ctx.info("Reusing the recipe extracted in the previous attempt")
                .await;
            recipe.0
        }
        None => {
            ctx.enter(Stage::Extract).await?;
            let images = if info.images.is_empty() {
                Vec::new()
            } else {
                ctx.info(format!(
                    "Attaching {} image(s) for the model",
                    info.images.len()
                ))
                .await;
                media::fetch_images_as_data_urls(&state.http, &info.images).await
            };
            ctx.info(format!("Extracting recipe with {}", config.text_model))
                .await;
            let openai = OpenAi {
                http: &state.http,
                config,
            };
            let extracted = openai
                .extract_recipe(&ExtractInput {
                    url: &job.url,
                    title: info.title.as_deref(),
                    uploader: info.uploader.as_deref(),
                    description: info.description.as_deref(),
                    transcript: transcript.as_deref().filter(|t| !t.is_empty()),
                    tags,
                    note: job.note.as_deref(),
                    images: &images,
                })
                .await?;
            let recipe = extracted.recipe;
            if recipe["is_recipe"] == false {
                let reason = recipe["not_recipe_reason"]
                    .as_str()
                    .unwrap_or("no reason given");
                bail!("no recipe found in this post: {reason}");
            }
            let name = recipe["name"].as_str().unwrap_or("Untitled recipe");
            db::set_recipe(
                &state.db,
                job.id,
                &recipe,
                name,
                extracted.prompt_tokens,
                extracted.completion_tokens,
            )
            .await?;
            ctx.info(format!(
                "Extracted \"{name}\": {} ingredients, {} steps ({} tokens)",
                recipe["recipeIngredient"].as_array().map_or(0, Vec::len),
                recipe["recipeInstructions"].as_array().map_or(0, Vec::len),
                extracted.prompt_tokens.unwrap_or(0) + extracted.completion_tokens.unwrap_or(0),
            ))
            .await;
            recipe
        }
    };

    ctx.enter(Stage::Import).await?;
    ctx.info("Sending recipe to Mealie").await;
    let ld = mealie::to_json_ld(&recipe, &job.url, info.thumbnail.as_deref(), tags);
    let slug = Mealie {
        http: &state.http,
        config,
    }
    .create_from_json_ld(&ld, &job.url)
    .await?;
    ctx.info(format!("Created {}", config.mealie_recipe_link(&slug)))
        .await;
    Ok(slug)
}

async fn download(
    ctx: &Ctx<'_>,
    media: &Media<'_>,
    url: &str,
    dir: &std::path::Path,
) -> Result<std::path::PathBuf> {
    ctx.info("Downloading audio").await;
    let (tx, mut rx) = tokio::sync::watch::channel(0.0_f64);
    let reporter = async {
        let mut last = -1.0;
        while rx.changed().await.is_ok() {
            let value = *rx.borrow_and_update();
            if value - last >= 0.02 || value >= 1.0 {
                last = value;
                ctx.progress(Some(value)).await;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    };
    let download = async {
        let result = media
            .download_audio(url, dir, |p| {
                let _ = tx.send(p);
            })
            .await;
        drop(tx);
        result
    };
    let (source, ()) = tokio::join!(download, reporter);
    let source = source?;
    let size = tokio::fs::metadata(&source)
        .await
        .map(|m| m.len())
        .unwrap_or(0);
    ctx.info(format!(
        "Downloaded {:.1} MB, converting for transcription",
        size as f64 / 1e6
    ))
    .await;
    ctx.progress(None).await;
    media.to_speech_mp3(&source, dir).await
}
