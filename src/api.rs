use axum::{
    extract::{Path, Query, Request, State},
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
    urls,
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
    Router::new()
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
    let tags: Vec<String> = req
        .tags
        .iter()
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .collect();
    let note = req.note.as_deref().map(str::trim).filter(|n| !n.is_empty());
    let id = db::insert_job(&state.db, &url, source, &tags, note).await?;
    state.wake.notify_waiters();
    state.publish_job(id).await;
    let job = db::get_summary(&state.db, id)
        .await?
        .ok_or_else(ApiError::not_found)?;
    Ok((StatusCode::CREATED, Json(job)))
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
