use axum::{
    extract::{DefaultBodyLimit, FromRequest, Multipart, Path, Query, Request, State},
    http::{header, StatusCode},
    middleware::Next,
    response::{
        sse::{self, KeepAlive, Sse},
        IntoResponse, Response,
    },
    routing::{get, post},
    Json, Router,
};
use futures::{Stream, StreamExt};
use serde::Deserialize;
use serde_json::json;
use std::convert::Infallible;
use tokio_stream::wrappers::{errors::BroadcastStreamRecvError, BroadcastStream};

use crate::{
    db,
    mealie::Mealie,
    state::{AppState, Update},
    uploads, urls,
};

pub struct ApiError(StatusCode, serde_json::Value);

impl ApiError {
    fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self(status, json!({ "error": message.into() }))
    }

    fn not_found() -> Self {
        Self::new(StatusCode::NOT_FOUND, "job not found")
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        tracing::error!("request failed: {e:#}");
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"))
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(self.1)).into_response()
    }
}

type ApiResult<T> = Result<T, ApiError>;

pub fn router(state: &AppState) -> Router<AppState> {
    let upload_limit = state.config.max_upload_bytes;
    Router::new()
        .route(
            "/api/jobs/upload",
            post(upload).layer(DefaultBodyLimit::max(upload_limit)),
        )
        .route("/healthz", get(|| async { "ok" }))
        .route("/api/config", get(config))
        .route("/api/stats", get(stats))
        .route("/api/events", get(events))
        .route("/api/jobs", get(list).post(create).delete(clear))
        .route("/api/jobs/{id}", get(detail).delete(remove))
        .route("/api/jobs/{id}/retry", post(retry))
        .route("/api/jobs/{id}/cancel", post(cancel))
        .route("/api/clean", post(clean_one))
        .route(
            "/api/clean/library",
            get(library_status).post(clean_library),
        )
        .route("/api/reimport", post(reimport_one))
        .route(
            "/api/reimport/library",
            get(reimport_status).post(reimport_library),
        )
        .route("/api/token", get(token))
        .route("/api/token/rotate", post(rotate_token))
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Paths anyone may fetch: the health check, and the icons and manifest a
/// phone fetches without credentials when the UI is added to the home screen.
const PUBLIC_PATHS: &[&str] = &[
    "/healthz",
    "/manifest.webmanifest",
    "/icon.svg",
    "/icon-192.png",
    "/icon-512.png",
    "/apple-touch-icon.png",
];

/// Guards every route. A Bearer header must carry the API token. With
/// `AUTH_PASSWORD` set, everything else needs HTTP Basic auth with that
/// password (any username). Without it, requests with no Authorization header
/// pass, which assumes a reverse proxy in front does the login.
pub async fn auth(State(state): State<AppState>, req: Request, next: Next) -> Response {
    if PUBLIC_PATHS.contains(&req.uri().path()) {
        return next.run(req).await;
    }
    let header = req
        .headers()
        .get(header::AUTHORIZATION)
        .map(|v| v.to_str().unwrap_or_default().trim());
    let password = state.config.auth_password.as_deref();
    let allowed = match (header, password) {
        (Some(value), _) if value.starts_with("Bearer ") => {
            let presented = value["Bearer ".len()..].trim();
            let token = state.api_token.read().unwrap();
            if !constant_time_eq(presented.as_bytes(), token.as_bytes()) {
                return ApiError::new(StatusCode::UNAUTHORIZED, "invalid API token")
                    .into_response();
            }
            true
        }
        (Some(value), Some(password)) => basic_password(value)
            .is_some_and(|p| constant_time_eq(p.as_bytes(), password.as_bytes())),
        (None, Some(_)) => false,
        (Some(_), None) => {
            return ApiError::new(StatusCode::UNAUTHORIZED, "invalid API token").into_response()
        }
        (None, None) => true,
    };
    if !allowed {
        return (
            StatusCode::UNAUTHORIZED,
            [(header::WWW_AUTHENTICATE, "Basic realm=\"Mealie Forager\"")],
            "authentication required",
        )
            .into_response();
    }
    next.run(req).await
}

/// The password from an HTTP Basic `Authorization` header value.
fn basic_password(value: &str) -> Option<String> {
    use base64::Engine;
    let encoded = value.strip_prefix("Basic ")?.trim();
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .ok()?;
    let decoded = String::from_utf8(decoded).ok()?;
    decoded.split_once(':').map(|(_, p)| p.to_string())
}

async fn token(State(state): State<AppState>) -> Json<serde_json::Value> {
    let token = state.api_token.read().unwrap().clone();
    Json(json!({ "token": token }))
}

async fn rotate_token(State(state): State<AppState>) -> ApiResult<Json<serde_json::Value>> {
    let token = db::rotate_api_token(&state.db).await?;
    *state.api_token.write().unwrap() = token.clone();
    Ok(Json(json!({ "token": token })))
}

async fn config(State(state): State<AppState>) -> Json<serde_json::Value> {
    let c = &state.config;
    Json(json!({
        "mealie_url": c.mealie_public_url,
        "mealie_group": c.mealie_group,
        "text_model": c.text_model,
        "transcription_model": c.transcription_model,
        "workers": c.workers,
        "max_duration_secs": c.max_duration_secs,
        "cleanup": c.cleanup,
        "clean_tag": c.clean_tag,
    }))
}

async fn stats(State(state): State<AppState>) -> ApiResult<Json<db::Stats>> {
    Ok(Json(db::stats(&state.db).await?))
}

#[derive(Deserialize)]
struct ListQuery {
    filter: Option<String>,
    limit: Option<i64>,
}

async fn list(
    State(state): State<AppState>,
    Query(q): Query<ListQuery>,
) -> ApiResult<Json<Vec<db::Job>>> {
    let limit = q.limit.unwrap_or(100).clamp(1, 500);
    Ok(Json(
        db::list(&state.db, q.filter.as_deref().unwrap_or("all"), limit).await?,
    ))
}

#[derive(Deserialize)]
struct CreateJob {
    url: String,
    #[serde(default)]
    tags: Vec<String>,
    note: Option<String>,
    /// "auto" (default), "social" (yt-dlp and the model) or "web" (Mealie's scraper).
    source: Option<String>,
    #[serde(default)]
    force: bool,
}

async fn create(
    State(state): State<AppState>,
    Json(req): Json<CreateJob>,
) -> ApiResult<(StatusCode, Json<db::Job>)> {
    enqueue_url(&state, req).await
}

fn clean_tags<'a>(tags: impl IntoIterator<Item = &'a String>) -> Vec<String> {
    tags.into_iter()
        .flat_map(|t| t.split(','))
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .collect()
}

async fn created(state: &AppState, id: i64) -> ApiResult<(StatusCode, Json<db::Job>)> {
    state.wake.notify_waiters();
    state.publish_job(id).await;
    let job = db::get_summary(&state.db, id)
        .await?
        .ok_or_else(ApiError::not_found)?;
    Ok((StatusCode::CREATED, Json(job)))
}

async fn enqueue_url(state: &AppState, req: CreateJob) -> ApiResult<(StatusCode, Json<db::Job>)> {
    let url = urls::extract(&req.url)
        .ok_or_else(|| ApiError::new(StatusCode::BAD_REQUEST, "that doesn't look like a link"))?;
    // A link to a recipe already in this Mealie means "clean it", not "import it".
    if let Some(slug) = state.config.mealie_slug_from_url(&url) {
        return enqueue_clean(state, &slug).await;
    }
    let source = match req.source.as_deref().unwrap_or("auto") {
        "auto" if urls::is_social(&url) => "social",
        "auto" => "web",
        s @ ("social" | "web") => s,
        _ => {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                "source must be auto, social or web",
            ))
        }
    };
    if !req.force {
        if let Some(existing) = db::find_imported(&state.db, &url).await? {
            return Err(ApiError(
                StatusCode::CONFLICT,
                json!({ "error": "already imported", "existing": existing }),
            ));
        }
    }
    let tags = clean_tags(&req.tags);
    let note = req.note.as_deref().map(str::trim).filter(|n| !n.is_empty());
    let id = db::insert_job(&state.db, &url, source, &tags, note).await?;
    created(state, id).await
}

fn mealie(state: &AppState) -> Mealie<'_> {
    Mealie {
        http: &state.http,
        config: &state.config,
    }
}

fn require_cleanup(state: &AppState) -> Result<(), ApiError> {
    if state.config.cleanup {
        Ok(())
    } else {
        Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "cleanup is turned off (CLEANUP=false)",
        ))
    }
}

fn has_clean_tag(recipe: &serde_json::Value, tag: &str) -> bool {
    recipe["tags"].as_array().is_some_and(|t| {
        t.iter().any(|t| {
            t["name"]
                .as_str()
                .is_some_and(|n| n.eq_ignore_ascii_case(tag))
        })
    })
}

async fn insert_clean(state: &AppState, recipe: &serde_json::Value) -> anyhow::Result<Option<i64>> {
    let Some(slug) = recipe["slug"].as_str() else {
        return Ok(None);
    };
    let link = state.config.mealie_recipe_link(slug);
    let thumbnail = recipe["id"]
        .as_str()
        .map(|id| state.config.mealie_image_link(id));
    let id = db::insert_clean_job(
        &state.db,
        &db::CleanJob {
            slug,
            link: &link,
            name: recipe["name"].as_str(),
            thumbnail: thumbnail.as_deref(),
        },
    )
    .await?;
    Ok(Some(id))
}

/// Queues a clean-only job for one recipe already in Mealie.
async fn enqueue_clean(state: &AppState, slug: &str) -> ApiResult<(StatusCode, Json<db::Job>)> {
    require_cleanup(state)?;
    if db::pending_clean_slugs(&state.db)
        .await?
        .iter()
        .any(|s| s == slug)
    {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "that recipe is already queued for cleaning",
        ));
    }
    let recipe = mealie(state).recipe(slug).await?.ok_or_else(|| {
        ApiError::new(StatusCode::NOT_FOUND, format!("no recipe {slug} in Mealie"))
    })?;
    let id = insert_clean(state, &recipe).await?.ok_or_else(|| {
        ApiError::new(
            StatusCode::BAD_GATEWAY,
            "Mealie returned a recipe without a slug",
        )
    })?;
    created(state, id).await
}

#[derive(Deserialize)]
struct CleanRequest {
    slug: Option<String>,
    url: Option<String>,
}

async fn clean_one(
    State(state): State<AppState>,
    Json(req): Json<CleanRequest>,
) -> ApiResult<(StatusCode, Json<db::Job>)> {
    let slug = req
        .slug
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| {
            req.url
                .as_deref()
                .and_then(|u| state.config.mealie_slug_from_url(u.trim()))
        })
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::BAD_REQUEST,
                "send a recipe slug or a link to it in Mealie",
            )
        })?;
    enqueue_clean(&state, &slug).await
}

/// Recipes in Mealie without the clean tag that aren't already queued.
async fn uncleaned(state: &AppState) -> ApiResult<(usize, Vec<serde_json::Value>)> {
    let recipes = mealie(state).recipes().await?;
    let pending = db::pending_clean_slugs(&state.db).await?;
    let total = recipes.len();
    let todo = recipes
        .into_iter()
        .filter(|r| !has_clean_tag(r, &state.config.clean_tag))
        .filter(|r| {
            r["slug"]
                .as_str()
                .is_some_and(|s| !pending.iter().any(|p| p == s))
        })
        .collect();
    Ok((total, todo))
}

async fn library_status(State(state): State<AppState>) -> ApiResult<Json<serde_json::Value>> {
    require_cleanup(&state)?;
    let (total, todo) = uncleaned(&state).await?;
    let queued = db::pending_clean_slugs(&state.db).await?.len();
    Ok(Json(json!({
        "total": total,
        "uncleaned": todo.len(),
        "queued": queued,
        "tag": state.config.clean_tag,
    })))
}

async fn clean_library(State(state): State<AppState>) -> ApiResult<Json<serde_json::Value>> {
    require_cleanup(&state)?;
    let (_, todo) = uncleaned(&state).await?;
    let mut queued = 0;
    for recipe in &todo {
        if insert_clean(&state, recipe).await?.is_some() {
            queued += 1;
        }
    }
    tracing::info!("queued {queued} Mealie recipes for cleaning");
    state.wake.notify_waiters();
    let _ = state.updates.send(Update::Refresh);
    Ok(Json(json!({ "queued": queued })))
}

/// The link a Mealie recipe was imported from, unless it points back at Mealie.
fn original_url(state: &AppState, recipe: &serde_json::Value) -> Option<String> {
    let url = urls::normalize(recipe["orgURL"].as_str()?)?;
    state
        .config
        .mealie_slug_from_url(&url)
        .is_none()
        .then_some(url)
}

/// Queues a re-import of a recipe's original link that replaces the recipe.
async fn insert_reimport(
    state: &AppState,
    recipe: &serde_json::Value,
) -> anyhow::Result<Option<i64>> {
    let (Some(slug), Some(url)) = (recipe["slug"].as_str(), original_url(state, recipe)) else {
        return Ok(None);
    };
    let source = if urls::is_social(&url) {
        "social"
    } else {
        "web"
    };
    // Tags carry over; the clean tag comes back once the new recipe is cleaned.
    let tags: Vec<String> = recipe["tags"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|t| t["name"].as_str())
        .filter(|n| !n.eq_ignore_ascii_case(&state.config.clean_tag))
        .map(str::to_string)
        .collect();
    let id = db::insert_reimport_job(&state.db, &url, source, &tags, slug).await?;
    Ok(Some(id))
}

async fn reimport_one(
    State(state): State<AppState>,
    Json(req): Json<CleanRequest>,
) -> ApiResult<(StatusCode, Json<db::Job>)> {
    let slug = req
        .slug
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| {
            req.url
                .as_deref()
                .and_then(|u| state.config.mealie_slug_from_url(u.trim()))
        })
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::BAD_REQUEST,
                "send a recipe slug or a link to it in Mealie",
            )
        })?;
    if db::pending_reimport_slugs(&state.db).await?.contains(&slug) {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "that recipe is already queued for re-import",
        ));
    }
    let recipe = mealie(&state).recipe(&slug).await?.ok_or_else(|| {
        ApiError::new(StatusCode::NOT_FOUND, format!("no recipe {slug} in Mealie"))
    })?;
    let id = insert_reimport(&state, &recipe).await?.ok_or_else(|| {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            "that recipe has no original link to import from",
        )
    })?;
    created(&state, id).await
}

/// Mealie recipes with an original link that aren't already queued for re-import.
async fn reimportable(state: &AppState) -> ApiResult<(usize, Vec<serde_json::Value>)> {
    let recipes = mealie(state).recipes().await?;
    let pending = db::pending_reimport_slugs(&state.db).await?;
    let total = recipes.len();
    let todo = recipes
        .into_iter()
        .filter(|r| original_url(state, r).is_some())
        .filter(|r| {
            r["slug"]
                .as_str()
                .is_some_and(|s| !pending.iter().any(|p| p == s))
        })
        .collect();
    Ok((total, todo))
}

async fn reimport_status(State(state): State<AppState>) -> ApiResult<Json<serde_json::Value>> {
    let (total, todo) = reimportable(&state).await?;
    let queued = db::pending_reimport_slugs(&state.db).await?.len();
    Ok(Json(json!({
        "total": total,
        "reimportable": todo.len(),
        "queued": queued,
    })))
}

async fn reimport_library(State(state): State<AppState>) -> ApiResult<Json<serde_json::Value>> {
    let (_, todo) = reimportable(&state).await?;
    let mut queued = 0;
    for recipe in &todo {
        if insert_reimport(&state, recipe).await?.is_some() {
            queued += 1;
        }
    }
    tracing::info!("queued {queued} Mealie recipes for re-import");
    state.wake.notify_waiters();
    let _ = state.updates.send(Update::Refresh);
    Ok(Json(json!({ "queued": queued })))
}

/// One field of an upload request, whatever format it arrived in.
struct Part {
    name: String,
    file_name: Option<String>,
    content_type: Option<String>,
    bytes: Vec<u8>,
}

impl Part {
    fn text(name: &str, value: impl Into<String>) -> Self {
        Part {
            name: name.into(),
            file_name: None,
            content_type: None,
            bytes: value.into().into_bytes(),
        }
    }
}

fn json_parts(value: serde_json::Value) -> Vec<Part> {
    let mut parts = Vec::new();
    for (key, value) in value.as_object().into_iter().flatten() {
        let values = match value {
            serde_json::Value::Array(items) => items.clone(),
            other => vec![other.clone()],
        };
        for v in values {
            match v {
                serde_json::Value::String(s) => parts.push(Part::text(key, s)),
                serde_json::Value::Bool(b) => parts.push(Part::text(key, b.to_string())),
                serde_json::Value::Number(n) => parts.push(Part::text(key, n.to_string())),
                _ => {}
            }
        }
    }
    parts
}

fn form_parts(bytes: &[u8]) -> Vec<Part> {
    url::form_urlencoded::parse(bytes)
        .map(|(k, v)| Part::text(&k, v.into_owned()))
        .collect()
}

/// Reads the request as multipart, URL-encoded or JSON fields, or else as one
/// raw file (optional fields then come from the query string).
async fn read_parts(state: &AppState, req: Request) -> Result<Vec<Part>, ApiError> {
    let bad = |m: String| ApiError::new(StatusCode::BAD_REQUEST, m);
    let content_type = req
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    let mime = content_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    if mime == "multipart/form-data" {
        let mut form = Multipart::from_request(req, state)
            .await
            .map_err(|e| {
                bad(format!(
                    "could not read the upload: {} (don't set a Content-Type header yourself; let the client add it)",
                    e.body_text()
                ))
            })?;
        let mut parts = Vec::new();
        while let Some(field) = form
            .next_field()
            .await
            .map_err(|e| bad(format!("could not read the upload: {e}")))?
        {
            parts.push(Part {
                name: field.name().unwrap_or_default().to_string(),
                file_name: field.file_name().map(str::to_string),
                content_type: field.content_type().map(str::to_string),
                bytes: field
                    .bytes()
                    .await
                    .map_err(|e| bad(format!("could not read the upload: {e}")))?
                    .to_vec(),
            });
        }
        return Ok(parts);
    }
    let query = req.uri().query().unwrap_or_default().to_string();
    let bytes = axum::body::to_bytes(req.into_body(), state.config.max_upload_bytes)
        .await
        .map_err(|e| bad(format!("could not read the upload: {e}")))?;
    let mut parts = match mime.as_str() {
        "application/x-www-form-urlencoded" => form_parts(&bytes),
        "application/json" => json_parts(
            serde_json::from_slice(&bytes).map_err(|e| bad(format!("invalid JSON: {e}")))?,
        ),
        _ => vec![Part {
            name: "file".into(),
            file_name: None,
            content_type: Some(content_type).filter(|c| !c.is_empty()),
            bytes: bytes.to_vec(),
        }],
    };
    parts.extend(form_parts(query.as_bytes()));
    Ok(parts)
}

/// Import from files (photos, screenshots, recipe text, a Mealie .zip, a video
/// or voice memo) or a `url`/`text` field. A link sent any of these ways becomes
/// a normal link job; plain text is imported as a recipe.
async fn upload(
    State(state): State<AppState>,
    req: Request,
) -> ApiResult<(StatusCode, Json<db::Job>)> {
    let bad = |m: String| ApiError::new(StatusCode::BAD_REQUEST, m);
    let mut uploads: Vec<uploads::Upload> = Vec::new();
    let mut links: Vec<String> = Vec::new();
    let mut tags: Vec<String> = Vec::new();
    let (mut note, mut source, mut force) = (None, None, false);
    for part in read_parts(&state, req).await? {
        let as_text = || String::from_utf8_lossy(&part.bytes).trim().to_string();
        match part.name.as_str() {
            "tags" => tags.push(as_text()),
            "note" => note = Some(as_text()).filter(|n| !n.is_empty()),
            "source" => source = Some(as_text()),
            "force" => force = matches!(as_text().as_str(), "true" | "1" | "yes" | "on"),
            _ => {
                if part.bytes.is_empty() {
                    continue;
                }
                let from_type = part.content_type.as_deref().and_then(|t| {
                    let (top, sub) = t.split(';').next()?.trim().split_once('/')?;
                    matches!(top, "image" | "video" | "audio")
                        .then(|| format!("upload.{}", sub.trim_start_matches("x-")))
                });
                let label =
                    part.file_name.clone().or(from_type).unwrap_or_else(
                        || match uploads::classify("", part.content_type.as_deref(), &part.bytes) {
                            Some(uploads::Classified::File(uploads::Kind::Image)) => "image".into(),
                            Some(uploads::Classified::File(uploads::Kind::Media)) => "media".into(),
                            Some(uploads::Classified::File(uploads::Kind::Zip)) => {
                                "export.zip".into()
                            }
                            _ => "pasted.txt".into(),
                        },
                    );
                match uploads::classify(&label, part.content_type.as_deref(), &part.bytes) {
                    Some(uploads::Classified::Link(url)) => links.push(url),
                    Some(uploads::Classified::File(kind)) => uploads.push(uploads::Upload {
                        name: label,
                        kind,
                        bytes: part.bytes,
                    }),
                    None => {
                        return Err(bad(format!(
                            "{label}: unsupported file (send photos, screenshots, text, a Mealie .zip, or a video/audio file)"
                        )))
                    }
                }
            }
        }
    }

    if uploads.is_empty() {
        let Some(url) = links.into_iter().next() else {
            return Err(bad("nothing to import: attach a file or send a link".into()));
        };
        let req = CreateJob {
            url,
            tags,
            note,
            source,
            force,
        };
        return enqueue_url(&state, req).await;
    }

    let kinds: Vec<uploads::Kind> = uploads.iter().map(|u| u.kind).collect();
    let media_kind = uploads::media_kind(&kinds).map_err(|e| bad(format!("{e:#}")))?;
    let tags = clean_tags(&tags);
    let first = uploads[0].name.clone();
    let title = if uploads.len() == 1 {
        first.clone()
    } else {
        format!("{first} + {} more", uploads.len() - 1)
    };

    // The worker can't claim the job until the files are on disk and this commits.
    let mut tx = state.db.begin().await.map_err(anyhow::Error::from)?;
    let id = db::insert_job(
        &mut *tx,
        &format!("upload:{first}"),
        "file",
        &tags,
        note.as_deref(),
    )
    .await?;
    sqlx::query("UPDATE jobs SET title = ?, platform = 'Upload', media_kind = ? WHERE id = ?")
        .bind(&title)
        .bind(media_kind)
        .bind(id)
        .execute(&mut *tx)
        .await
        .map_err(anyhow::Error::from)?;
    uploads::store(&state.config.upload_dir, id, &uploads).await?;
    tx.commit().await.map_err(anyhow::Error::from)?;
    created(&state, id).await
}

async fn detail(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> ApiResult<Json<serde_json::Value>> {
    let job = db::get_full(&state.db, id)
        .await?
        .ok_or_else(ApiError::not_found)?;
    Ok(Json(json!({
        "job": job,
        "stages": db::stages(&state.db, id).await?,
        "events": db::events(&state.db, id).await?,
    })))
}

#[derive(Deserialize, Default)]
struct RetryBody {
    #[serde(default)]
    fresh: bool,
}

async fn retry(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    body: Option<Json<RetryBody>>,
) -> ApiResult<StatusCode> {
    let fresh = body.map(|b| b.0).unwrap_or_default().fresh;
    if !db::requeue(&state.db, id, fresh).await? {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "job is queued or running",
        ));
    }
    let message = if fresh {
        "Queued for a fresh retry"
    } else {
        "Queued for retry"
    };
    if let Ok(event) = db::log(&state.db, id, "info", None, message).await {
        state.publish_event(event);
    }
    state.wake.notify_waiters();
    state.publish_job(id).await;
    Ok(StatusCode::ACCEPTED)
}

async fn cancel(State(state): State<AppState>, Path(id): Path<i64>) -> ApiResult<StatusCode> {
    let token = state.running.lock().unwrap().get(&id).cloned();
    if let Some(token) = token {
        token.cancel();
    } else if db::cancel_queued(&state.db, id).await? {
        state.publish_job(id).await;
    } else {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "job is not queued or running",
        ));
    }
    Ok(StatusCode::ACCEPTED)
}

async fn remove(State(state): State<AppState>, Path(id): Path<i64>) -> ApiResult<StatusCode> {
    if !db::delete(&state.db, id).await? {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "running jobs can't be deleted",
        ));
    }
    let dir = uploads::job_dir(&state.config.upload_dir, id);
    if let Err(e) = tokio::fs::remove_dir_all(&dir).await {
        if e.kind() != std::io::ErrorKind::NotFound {
            tracing::warn!("could not remove {}: {e}", dir.display());
        }
    }
    let _ = state.updates.send(Update::Deleted { id });
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct ClearQuery {
    status: Option<String>,
}

async fn clear(
    State(state): State<AppState>,
    Query(q): Query<ClearQuery>,
) -> ApiResult<Json<serde_json::Value>> {
    let removed = db::clear_finished(&state.db, q.status.as_deref().unwrap_or("finished")).await?;
    if let Err(e) = uploads::prune(&state.db, &state.config.upload_dir).await {
        tracing::warn!("could not prune uploads: {e:#}");
    }
    let _ = state.updates.send(Update::Refresh);
    Ok(Json(json!({ "removed": removed })))
}

async fn events(
    State(state): State<AppState>,
) -> (
    [(header::HeaderName, &'static str); 1],
    Sse<impl Stream<Item = Result<sse::Event, Infallible>>>,
) {
    let stream = BroadcastStream::new(state.updates.subscribe()).map(|msg| {
        let update = match msg {
            Ok(update) => update,
            Err(BroadcastStreamRecvError::Lagged(_)) => Update::Refresh,
        };
        Ok(sse::Event::default()
            .json_data(&update)
            .unwrap_or_else(|_| sse::Event::default().comment("serialize error")))
    });
    (
        [(header::HeaderName::from_static("x-accel-buffering"), "no")],
        Sse::new(stream).keep_alive(KeepAlive::default()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_basic_auth_password() {
        // "anyone:s3cret:with-colon"
        assert_eq!(
            basic_password("Basic YW55b25lOnMzY3JldDp3aXRoLWNvbG9u").as_deref(),
            Some("s3cret:with-colon")
        );
        assert_eq!(basic_password("Basic !!!"), None);
        assert_eq!(basic_password("Bearer abc"), None);
    }
}
