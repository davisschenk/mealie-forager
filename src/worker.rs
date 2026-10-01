use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    sync::Mutex,
    time::{Duration, Instant},
};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use crate::{
    clean,
    db::{self, Job, Stage, Status},
    mealie::{self, Mealie},
    media::{self, Media, MediaInfo},
    openai::{self, ExtractInput, OpenAi},
    state::AppState,
    uploads,
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
            ctx.info(format!("Finished in {elapsed}")).await;
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
    let job = db::get_full(&state.db, ctx.id)
        .await?
        .ok_or_else(|| anyhow!("job disappeared"))?;
    let mealie = Mealie {
        http: &state.http,
        config: &state.config,
    };

    // An earlier attempt already created the recipe; only the cleanup is left.
    let existing = match &job.mealie_slug {
        Some(slug) if mealie.recipe(slug).await?.is_some() => {
            ctx.info(format!(
                "Recipe {slug} is already in Mealie; skipping the import"
            ))
            .await;
            Some(slug.clone())
        }
        Some(slug) if job.source == "mealie" => {
            bail!("recipe {slug} is no longer in Mealie");
        }
        Some(slug) => {
            ctx.warn(format!(
                "Recipe {slug} is no longer in Mealie; importing again"
            ))
            .await;
            None
        }
        None => None,
    };
    if let Some(old) = &job.replaces_slug {
        ctx.info(format!(
            "Re-importing; {old} is replaced once the new recipe is in Mealie"
        ))
        .await;
    }
    let slug = match existing {
        Some(slug) => slug,
        None => import(ctx, &job, &mealie).await?,
    };
    let slug = if state.config.cleanup {
        if job.source == "mealie" {
            ctx.info("Cleaning a recipe already in Mealie").await;
        }
        clean_recipe(ctx, &mealie, &slug, job.note.as_deref()).await?
    } else {
        slug
    };
    match &job.replaces_slug {
        Some(old) => replace_old(ctx, &mealie, old, slug).await,
        None => Ok(slug),
    }
}

/// Deletes the recipe a re-import replaces, now that the new one is in place.
async fn replace_old(
    ctx: &Ctx<'_>,
    mealie: &Mealie<'_>,
    old: &str,
    slug: String,
) -> Result<String> {
    let db = &ctx.state.db;
    if old == slug {
        db::clear_replaces(db, ctx.id).await?;
        return Ok(slug);
    }
    if let Err(e) = mealie.delete_recipe(old).await {
        ctx.warn(format!(
            "Could not delete the old recipe {old}, so both are in Mealie: {e:#}"
        ))
        .await;
        return Ok(slug);
    }
    db::clear_replaces(db, ctx.id).await?;
    ctx.info(format!("Deleted the old recipe {old}")).await;
    // The new recipe got a suffixed slug while the old one existed; saving it
    // again lets the slug follow its name now that the old slug is free.
    let Some(recipe) = mealie.recipe(&slug).await? else {
        return Ok(slug);
    };
    let saved = mealie.update_recipe(&slug, &recipe).await?;
    let slug = saved["slug"].as_str().unwrap_or(&slug).to_string();
    db::set_slug(db, ctx.id, &slug).await?;
    Ok(slug)
}

async fn import(ctx: &Ctx<'_>, job: &Job, mealie: &Mealie<'_>) -> Result<String> {
    if job.source == "file" {
        return import_upload(ctx, job, mealie).await;
    }
    // Web jobs fall back to the post pipeline when Mealie can't scrape them; the
    // metadata that fallback saves (a non-"web" media kind) keeps retries there.
    let web = job.source == "web" && job.media_kind.as_deref().is_none_or(|k| k == "web");
    if !web {
        return import_post(ctx, job, mealie, None).await;
    }
    ctx.enter(Stage::Import).await?;
    ctx.info(format!("Asking Mealie to import {}", job.url))
        .await;
    let created = match mealie.create_from_url(&job.url).await {
        Ok(slug) => require_complete(mealie, &slug).await.map(|()| slug),
        Err(e) => Err(e),
    };
    match created {
        Ok(slug) => {
            let host = url::Url::parse(&job.url).ok().and_then(|u| {
                u.host_str()
                    .map(|h| h.trim_start_matches("www.").to_string())
            });
            finish_mealie_import(ctx, job, mealie, &slug, "web", host.as_deref()).await
        }
        Err(e) => {
            let message = format!("{e:#}");
            db::finish_stage(&ctx.state.db, ctx.id, "error").await?;
            ctx.warn(format!(
                "Mealie could not import this page ({message}); trying it as a post"
            ))
            .await;
            import_post(ctx, job, mealie, Some(&message)).await
        }
    }
}

/// Deletes a recipe Mealie just created when it has no ingredients or no
/// instructions, so a failed job doesn't leave a half recipe behind.
async fn require_complete(mealie: &Mealie<'_>, slug: &str) -> Result<()> {
    let recipe = mealie
        .recipe(slug)
        .await?
        .ok_or_else(|| anyhow!("Mealie created {slug} but can't find it"))?;
    let missing = clean::missing_parts(&recipe);
    if missing.is_empty() {
        return Ok(());
    }
    let missing = missing.join(" or ");
    match mealie.delete_recipe(slug).await {
        Ok(()) => bail!("Mealie's import has no {missing}, so it was deleted"),
        Err(e) => bail!("Mealie's import has no {missing}, and deleting {slug} failed: {e:#}"),
    }
}

/// Records a recipe Mealie created itself and applies the job's tags; returns the slug.
async fn finish_mealie_import(
    ctx: &Ctx<'_>,
    job: &Job,
    mealie: &Mealie<'_>,
    slug: &str,
    media_kind: &str,
    platform: Option<&str>,
) -> Result<String> {
    let state = ctx.state;
    let config = &state.config;
    db::set_slug(&state.db, job.id, slug).await?;
    let mut recipe = mealie
        .recipe(slug)
        .await?
        .ok_or_else(|| anyhow!("Mealie created {slug} but can't find it"))?;
    let name = recipe["name"].as_str().unwrap_or(slug).to_string();
    let thumbnail = recipe["id"].as_str().map(|id| config.mealie_image_link(id));
    db::set_metadata(
        &state.db,
        job.id,
        &db::Metadata {
            title: Some(&name),
            platform,
            uploader: None,
            thumbnail: thumbnail.as_deref(),
            duration_secs: None,
            description: recipe["description"].as_str(),
            media_kind,
            images: &[],
        },
    )
    .await?;
    db::set_recipe_name(&state.db, job.id, &name).await?;
    ctx.info(format!(
        "Mealie imported \"{name}\": {} ingredients, {} steps",
        recipe["recipeIngredient"].as_array().map_or(0, Vec::len),
        recipe["recipeInstructions"].as_array().map_or(0, Vec::len),
    ))
    .await;

    if !job.tags.0.is_empty() {
        let mut tags = Vec::new();
        for name in &job.tags.0 {
            tags.push(mealie.ensure_tag(name).await?);
        }
        mealie::merge_tags(&mut recipe, &tags, false);
        let saved = mealie.update_recipe(slug, &recipe).await?;
        if let Some(new) = saved["slug"].as_str().filter(|s| *s != slug) {
            db::set_slug(&state.db, job.id, new).await?;
        }
        ctx.info(format!("Tagged with {}", job.tags.0.join(", ")))
            .await;
        recipe = saved;
    }
    let slug = recipe["slug"].as_str().unwrap_or(slug).to_string();
    ctx.info(format!("Created {}", config.mealie_recipe_link(&slug)))
        .await;
    Ok(slug)
}

/// Imports uploaded files: photos and text through Mealie's AI import, a Mealie
/// export through its zip import, and video/audio through transcription and
/// extraction here.
async fn import_upload(ctx: &Ctx<'_>, job: &Job, mealie: &Mealie<'_>) -> Result<String> {
    let state = ctx.state;
    let config = &state.config;
    let files = uploads::load(&config.upload_dir, job.id).await?;
    let kind = job.media_kind.clone().unwrap_or_default();
    let names = files
        .iter()
        .map(|(s, _)| s.name.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    match kind.as_str() {
        "zip" => {
            ctx.enter(Stage::Import).await?;
            ctx.info(format!("Importing the Mealie export {names}"))
                .await;
            let (stored, bytes) = files.into_iter().next().context("no file uploaded")?;
            let slug = mealie.create_from_zip(stored.name, bytes).await?;
            require_complete(mealie, &slug).await?;
            finish_mealie_import(ctx, job, mealie, &slug, &kind, Some("Upload")).await
        }
        "video" => import_upload_media(ctx, job, mealie, files).await,
        _ => {
            ctx.enter(Stage::Import).await?;
            let mut images = Vec::new();
            let mut texts = Vec::new();
            for (stored, bytes) in files {
                match stored.kind {
                    uploads::Kind::Image => {
                        images.push((stored.name.clone(), uploads::mime(&stored), bytes));
                    }
                    _ => texts.push(String::from_utf8_lossy(&bytes).into_owned()),
                }
            }
            ctx.info(format!(
                "Asking Mealie's AI import to read {} image(s) and {} text file(s): {names}",
                images.len(),
                texts.len()
            ))
            .await;
            let content = Some(texts.join("\n\n")).filter(|t| !t.trim().is_empty());
            let slug = mealie.create_with_ai(content, images).await?;
            require_complete(mealie, &slug).await?;
            finish_mealie_import(ctx, job, mealie, &slug, &kind, Some("Upload")).await
        }
    }
}

async fn import_upload_media(
    ctx: &Ctx<'_>,
    job: &Job,
    mealie: &Mealie<'_>,
    files: Vec<(uploads::Stored, Vec<u8>)>,
) -> Result<String> {
    let state = ctx.state;
    let config = &state.config;
    let (stored, bytes) = files.into_iter().next().context("no file uploaded")?;
    let openai = OpenAi {
        http: &state.http,
        config,
    };
    let recipe = match job.recipe_json.clone() {
        Some(recipe) => {
            ctx.info("Reusing the recipe extracted in the previous attempt")
                .await;
            recipe.0
        }
        None => {
            let transcript = match job.transcript.clone() {
                Some(t) => t,
                None => {
                    ctx.enter(Stage::Transcribe).await?;
                    let work = tempfile::Builder::new()
                        .prefix(&format!("job-{}-", job.id))
                        .tempdir_in(&config.work_dir)?;
                    let source = work.path().join(format!("input-{}", stored.file));
                    tokio::fs::write(&source, &bytes).await?;
                    ctx.info(format!("Converting {} for transcription", stored.name))
                        .await;
                    let media = Media {
                        config,
                        cookies: None,
                    };
                    let audio = media.to_speech_mp3(&source, work.path()).await?;
                    ctx.info(format!("Transcribing with {}", config.transcription_model))
                        .await;
                    let text = openai.transcribe(&audio).await?;
                    ctx.info(format!(
                        "Transcript has {} words",
                        text.split_whitespace().count()
                    ))
                    .await;
                    db::set_transcript(&state.db, job.id, &text).await?;
                    text
                }
            };
            ctx.enter(Stage::Extract).await?;
            ctx.info(format!("Extracting recipe with {}", config.text_model))
                .await;
            let extracted = openai
                .extract_recipe(&ExtractInput {
                    url: &job.url,
                    title: Some(&stored.name),
                    uploader: None,
                    description: None,
                    transcript: Some(transcript.as_str()).filter(|t| !t.is_empty()),
                    tags: &job.tags.0,
                    note: job.note.as_deref(),
                    images: &[],
                })
                .await?;
            let recipe = extracted.value;
            openai::check_recipe(&recipe, "recording")?;
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
            recipe
        }
    };
    ctx.enter(Stage::Import).await?;
    ctx.info("Sending recipe to Mealie").await;
    let ld = mealie::to_json_ld(&recipe, "", None, &job.tags.0);
    let slug = mealie.create_from_json_ld(&ld, "").await?;
    db::set_slug(&state.db, job.id, &slug).await?;
    ctx.info(format!("Created {}", config.mealie_recipe_link(&slug)))
        .await;
    Ok(slug)
}

async fn import_post(
    ctx: &Ctx<'_>,
    job: &Job,
    mealie: &Mealie<'_>,
    scrape_error: Option<&str>,
) -> Result<String> {
    let state = ctx.state;
    let config = &state.config;
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
                    let unread = format!(
                        "neither yt-dlp ({e:#}) nor gallery-dl ({g:#}) could read this post"
                    );
                    match scrape_error {
                        Some(s) => anyhow!("Mealie could not scrape the page ({s}), and {unread}"),
                        None => anyhow!(unread),
                    }
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
            let recipe = extracted.value;
            openai::check_recipe(&recipe, "post")?;
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
    let slug = mealie.create_from_json_ld(&ld, &job.url).await?;
    db::set_slug(&state.db, job.id, &slug).await?;
    ctx.info(format!("Created {}", config.mealie_recipe_link(&slug)))
        .await;
    Ok(slug)
}

/// Rebuilds the imported recipe with linked foods and units, tidy steps and
/// metadata, then tags it as cleaned.
async fn clean_recipe(
    ctx: &Ctx<'_>,
    mealie: &Mealie<'_>,
    slug: &str,
    note: Option<&str>,
) -> Result<String> {
    let state = ctx.state;
    let config = &state.config;
    ctx.enter(Stage::Clean).await?;
    let mut recipe = mealie
        .recipe(slug)
        .await?
        .ok_or_else(|| anyhow!("recipe {slug} is no longer in Mealie"))?;
    let missing = clean::missing_parts(&recipe);
    if !missing.is_empty() {
        bail!(
            "the recipe has no {}, so it was left untagged",
            missing.join(" or ")
        );
    }
    let lines = clean::original_lines(&recipe);
    let units = clean::usable_units(mealie.units().await?);
    let categories = mealie.categories().await?;
    ctx.info(format!(
        "Cleaning {} ingredient lines with {}",
        lines.len(),
        config.clean_model
    ))
    .await;

    let openai = OpenAi {
        http: &state.http,
        config,
    };
    let extra = [config.extra_prompt.as_deref(), note]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join("\n");
    let completion = openai
        .structured(
            &config.clean_model,
            clean::SYSTEM_PROMPT,
            json!(clean::prompt(
                &recipe,
                &lines,
                &units,
                &categories,
                Some(extra.as_str()).filter(|e| !e.is_empty())
            )),
            "recipe_cleanup",
            clean::plan_schema(),
        )
        .await?;
    db::add_tokens(
        &state.db,
        ctx.id,
        completion.prompt_tokens,
        completion.completion_tokens,
    )
    .await?;
    let plan: clean::Plan = serde_json::from_value(completion.value)
        .context("model returned an unexpected cleanup plan")?;
    if let Some(reason) = plan
        .cannot_clean
        .as_deref()
        .filter(|r| !r.trim().is_empty())
    {
        bail!("the recipe is too incomplete to clean: {reason}");
    }
    if plan.ingredients.is_empty() {
        bail!("the cleanup returned no ingredients");
    }

    let foods = resolve_foods(ctx, mealie, &openai, &plan).await?;
    let plan_units = resolve_units(ctx, mealie, &plan, &units).await?;
    let built = clean::build(&plan, &lines, &foods, &plan_units)?;
    for warning in &built.warnings {
        ctx.warn(warning).await;
    }
    for note in &plan.notes {
        ctx.warn(format!("Needs a human look: {note}")).await;
    }
    clean::apply(&mut recipe, &plan, built);
    let (added, unknown) = clean::add_categories(&mut recipe, &plan, &categories);
    if !added.is_empty() {
        ctx.info(format!("Categorized as {}", added.join(", ")))
            .await;
    }
    if !unknown.is_empty() {
        ctx.warn(format!(
            "Skipped categories that don't exist in Mealie: {}",
            unknown.join(", ")
        ))
        .await;
    }
    if categories.is_empty() {
        ctx.warn("Mealie has no categories yet, so the recipe wasn't categorized")
            .await;
    }
    let saved = mealie.update_recipe(slug, &recipe).await?;
    let slug = saved["slug"].as_str().unwrap_or(slug).to_string();
    db::set_slug(&state.db, ctx.id, &slug).await?;

    let mut saved = mealie
        .recipe(&slug)
        .await?
        .ok_or_else(|| anyhow!("recipe {slug} disappeared after the cleanup"))?;
    let mut known_units = units;
    known_units.extend(plan_units.into_values());
    let problems = clean::verify(&saved, &known_units);
    if !problems.is_empty() {
        bail!(
            "the cleanup didn't stick, so the recipe was left untagged: {}",
            problems.join("; ")
        );
    }

    let tag = mealie.ensure_tag(&config.clean_tag).await?;
    mealie::merge_tags(&mut saved, &[tag], true);
    let saved = mealie.update_recipe(&slug, &saved).await?;
    let slug = saved["slug"].as_str().unwrap_or(&slug).to_string();
    match mealie.delete_empty_hashtags().await {
        Ok(deleted) if !deleted.is_empty() => {
            ctx.info(format!("Deleted unused tags {}", deleted.join(", ")))
                .await;
        }
        Ok(_) => {}
        Err(e) => {
            ctx.warn(format!("Could not tidy unused hashtag tags: {e:#}"))
                .await
        }
    }
    let name = saved["name"].as_str().unwrap_or(&slug);
    db::set_recipe_name(&state.db, ctx.id, name).await?;
    ctx.info(format!(
        "Cleaned \"{name}\": {} ingredients, {} steps, tagged {}",
        saved["recipeIngredient"].as_array().map_or(0, Vec::len),
        saved["recipeInstructions"].as_array().map_or(0, Vec::len),
        config.clean_tag
    ))
    .await;
    Ok(slug)
}

/// Links every planned food to a Mealie food: exact name matches first, then the
/// model picks among search results, and only then are new foods created.
async fn resolve_foods(
    ctx: &Ctx<'_>,
    mealie: &Mealie<'_>,
    openai: &OpenAi<'_>,
    plan: &clean::Plan,
) -> Result<HashMap<String, Value>> {
    let mut resolved = HashMap::new();
    let mut plurals = HashMap::new();
    let mut pending: Vec<(String, Vec<Value>)> = Vec::new();
    for (name, plural) in plan.foods() {
        let food = clean::key(name);
        if food.is_empty() {
            bail!("the cleanup left an ingredient without a food");
        }
        if resolved.contains_key(&food) || plurals.contains_key(&food) {
            continue;
        }
        plurals.insert(food.clone(), clean::key(plural));
        let mut candidates: Vec<Value> = Vec::new();
        for term in clean::search_terms(&food) {
            for found in mealie.search_foods(&term).await? {
                if !candidates.iter().any(|c| c["id"] == found["id"]) {
                    candidates.push(found);
                }
            }
            if clean::exact_food(&candidates, &food, plural).is_some() {
                break;
            }
        }
        match clean::exact_food(&candidates, &food, plural) {
            Some(found) => {
                resolved.insert(food, found.clone());
            }
            None => pending.push((food, candidates)),
        }
    }

    let (ask, mut create): (Vec<_>, Vec<_>) = pending.into_iter().partition(|(_, c)| !c.is_empty());
    if !ask.is_empty() {
        let completion = openai
            .structured(
                &ctx.state.config.clean_model,
                clean::MATCH_PROMPT,
                json!(clean::match_prompt(&ask)),
                "food_match",
                clean::match_schema(),
            )
            .await?;
        db::add_tokens(
            &ctx.state.db,
            ctx.id,
            completion.prompt_tokens,
            completion.completion_tokens,
        )
        .await?;
        let matches: clean::Matches = serde_json::from_value(completion.value)
            .context("model returned unexpected food matches")?;
        for (food, candidates) in ask {
            let chosen = matches
                .matches
                .iter()
                .find(|m| clean::key(&m.food) == food)
                .and_then(|m| m.id.as_deref())
                .and_then(|id| candidates.iter().find(|c| c["id"].as_str() == Some(id)));
            match chosen {
                Some(found) => {
                    resolved.insert(food, found.clone());
                }
                None => create.push((food, candidates)),
            }
        }
    }
    for (food, _) in create {
        let plural = plurals
            .get(&food)
            .filter(|p| !p.is_empty())
            .cloned()
            .unwrap_or_else(|| food.clone());
        let created = mealie.create_food(&food, &plural).await?;
        ctx.info(format!("Created food \"{food}\"")).await;
        resolved.insert(food, created);
    }
    Ok(resolved)
}

/// Maps every planned unit to an existing Mealie unit, creating the genuinely
/// new ones the plan declared.
async fn resolve_units(
    ctx: &Ctx<'_>,
    mealie: &Mealie<'_>,
    plan: &clean::Plan,
    units: &[Value],
) -> Result<HashMap<String, Value>> {
    let mut resolved = HashMap::new();
    let mut created: Vec<Value> = Vec::new();
    for name in plan.ingredients.iter().filter_map(|i| i.unit.as_deref()) {
        let unit = clean::key(name);
        if unit.is_empty() || resolved.contains_key(&unit) {
            continue;
        }
        if let Some(found) =
            clean::find_unit(units, &unit).or_else(|| clean::find_unit(&created, &unit))
        {
            resolved.insert(unit, found.clone());
            continue;
        }
        let Some(new) = plan
            .new_units
            .iter()
            .find(|u| clean::key(&u.name) == unit || clean::key(&u.plural_name) == unit)
        else {
            continue;
        };
        let made = mealie
            .create_unit(
                &clean::key(&new.name),
                &clean::key(&new.plural_name),
                new.abbreviation.trim(),
            )
            .await?;
        ctx.info(format!("Created unit \"{}\"", clean::key(&new.name)))
            .await;
        created.push(made.clone());
        resolved.insert(unit, made);
    }
    Ok(resolved)
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
