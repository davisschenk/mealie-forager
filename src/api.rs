use axum::{
    extract::{Path, Query, State},
    http::{header, StatusCode},
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

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/api/config", get(config))
        .route("/api/stats", get(stats))
        .route("/api/events", get(events))
        .route("/api/jobs", get(list).post(create).delete(clear))
        .route("/api/jobs/{id}", get(detail).delete(remove))
        .route("/api/jobs/{id}/retry", post(retry))
        .route("/api/jobs/{id}/cancel", post(cancel))
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
    #[serde(default)]
    force: bool,
}

async fn create(
    State(state): State<AppState>,
    Json(req): Json<CreateJob>,
) -> ApiResult<(StatusCode, Json<db::Job>)> {
    let url = urls::extract(&req.url)
        .ok_or_else(|| ApiError::new(StatusCode::BAD_REQUEST, "that doesn't look like a link"))?;
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
    let id = db::insert_job(&state.db, &url, &tags, note).await?;
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
