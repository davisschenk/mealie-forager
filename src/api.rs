use axum::{
    extract::{DefaultBodyLimit, FromRequest, Multipart, Path, Query, Request, State},
    http::{header, StatusCode},
    middleware::{self, Next},
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

pub fn router(state: AppState) -> Router<AppState> {
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
        .route("/api/token", get(token))
        .route("/api/token/rotate", post(rotate_token))
        .layer(middleware::from_fn_with_state(state, bearer_auth))
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Requests carrying an Authorization header must present the API token. The
/// reverse proxy lets only those skip its login, so this is what guards them;
/// requests without the header are the browser UI behind the proxy's login.
async fn bearer_auth(State(state): State<AppState>, req: Request, next: Next) -> Response {
    if let Some(value) = req.headers().get(header::AUTHORIZATION) {
        let presented = value
            .to_str()
            .ok()
            .and_then(|v| v.strip_prefix("Bearer "))
            .map(str::trim)
            .unwrap_or_default();
        let valid = {
            let token = state.api_token.read().unwrap();
            constant_time_eq(presented.as_bytes(), token.as_bytes())
        };
        if !valid {
            return ApiError::new(StatusCode::UNAUTHORIZED, "invalid API token").into_response();
        }
    }
    next.run(req).await
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
