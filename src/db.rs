use anyhow::Result;
use serde::Serialize;
use sqlx::{
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions},
    types::Json,
    SqlitePool,
};
use std::{path::Path, str::FromStr, time::Duration};

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

pub async fn connect(path: &Path) -> Result<SqlitePool> {
    let options = SqliteConnectOptions::from_str(&format!("sqlite://{}", path.display()))?
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .foreign_keys(true)
        .busy_timeout(Duration::from_secs(10));
    open(options, 8).await
}

#[cfg(test)]
pub async fn connect_memory() -> Result<SqlitePool> {
    let options = SqliteConnectOptions::from_str("sqlite::memory:")?.foreign_keys(true);
    open(options, 1).await
}

async fn open(options: SqliteConnectOptions, max: u32) -> Result<SqlitePool> {
    let pool = SqlitePoolOptions::new()
        .max_connections(max)
        .connect_with(options)
        .await?;
    sqlx::migrate!("./migrations").run(&pool).await?;
    Ok(pool)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Succeeded,
    Failed,
    Cancelled,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Succeeded => "succeeded",
            Status::Failed => "failed",
            Status::Cancelled => "cancelled",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Stage {
    Queued,
    Metadata,
    Download,
    Transcribe,
    Extract,
    Import,
    Clean,
    Done,
}

impl Stage {
    pub fn as_str(self) -> &'static str {
        match self {
            Stage::Queued => "queued",
            Stage::Metadata => "metadata",
            Stage::Download => "download",
            Stage::Transcribe => "transcribe",
            Stage::Extract => "extract",
            Stage::Import => "import",
            Stage::Clean => "clean",
            Stage::Done => "done",
        }
    }
}

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct Job {
    pub id: i64,
    pub url: String,
    pub source: String,
    pub tags: Json<Vec<String>>,
    pub note: Option<String>,
    pub status: String,
    pub stage: String,
    pub progress: Option<f64>,
    pub attempts: i64,
    pub error: Option<String>,
    pub error_stage: Option<String>,
    pub title: Option<String>,
    pub platform: Option<String>,
    pub uploader: Option<String>,
    pub thumbnail: Option<String>,
    pub duration_secs: Option<f64>,
    pub media_kind: Option<String>,
    pub recipe_name: Option<String>,
    pub mealie_slug: Option<String>,
    pub prompt_tokens: Option<i64>,
    pub completion_tokens: Option<i64>,
    pub created_at: i64,
    pub started_at: Option<i64>,
    pub finished_at: Option<i64>,
    pub updated_at: i64,

    #[sqlx(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[sqlx(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub images: Option<Json<Vec<String>>>,
    #[sqlx(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transcript: Option<String>,
    #[sqlx(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recipe_json: Option<Json<serde_json::Value>>,
}

const SUMMARY_COLUMNS: &str =
    "id, url, source, tags, note, status, stage, progress, attempts, error, \
    error_stage, title, platform, uploader, thumbnail, duration_secs, media_kind, recipe_name, \
    mealie_slug, prompt_tokens, completion_tokens, created_at, started_at, finished_at, updated_at";

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct StageRun {
    pub attempt: i64,
    pub stage: String,
    pub outcome: String,
    pub started_at: i64,
    pub finished_at: Option<i64>,
}

#[derive(Debug, Clone, Serialize, sqlx::FromRow)]
pub struct Event {
    pub id: i64,
    pub job_id: i64,
    pub at: i64,
    pub level: String,
    pub stage: Option<String>,
    pub message: String,
}

pub async fn insert_job<'e>(
    db: impl sqlx::Executor<'e, Database = sqlx::Sqlite>,
    url: &str,
    source: &str,
    tags: &[String],
    note: Option<&str>,
) -> Result<i64> {
    let now = now_ms();
    let id = sqlx::query_scalar(
        "INSERT INTO jobs (url, source, tags, note, status, stage, created_at, updated_at) \
         VALUES (?, ?, ?, ?, 'queued', 'queued', ?, ?) RETURNING id",
    )
    .bind(url)
    .bind(source)
    .bind(Json(tags))
    .bind(note)
    .bind(now)
    .bind(now)
    .fetch_one(db)
    .await?;
    Ok(id)
}

pub async fn find_imported(db: &SqlitePool, url: &str) -> Result<Option<Job>> {
    Ok(sqlx::query_as(&format!(
        "SELECT {SUMMARY_COLUMNS} FROM jobs WHERE url = ? AND status = 'succeeded' \
         ORDER BY id DESC LIMIT 1"
    ))
    .bind(url)
    .fetch_optional(db)
    .await?)
}

pub async fn get_summary(db: &SqlitePool, id: i64) -> Result<Option<Job>> {
    Ok(
        sqlx::query_as(&format!("SELECT {SUMMARY_COLUMNS} FROM jobs WHERE id = ?"))
            .bind(id)
            .fetch_optional(db)
            .await?,
    )
}

pub async fn get_full(db: &SqlitePool, id: i64) -> Result<Option<Job>> {
    Ok(sqlx::query_as("SELECT * FROM jobs WHERE id = ?")
        .bind(id)
        .fetch_optional(db)
        .await?)
}

pub async fn list(db: &SqlitePool, filter: &str, limit: i64) -> Result<Vec<Job>> {
    let clause = match filter {
        "active" => "WHERE status IN ('queued', 'running')",
        "succeeded" => "WHERE status = 'succeeded'",
        "failed" => "WHERE status IN ('failed', 'cancelled')",
        _ => "",
    };
    Ok(sqlx::query_as(&format!(
        "SELECT {SUMMARY_COLUMNS} FROM jobs {clause} ORDER BY id DESC LIMIT ?"
    ))
    .bind(limit)
    .fetch_all(db)
    .await?)
}

pub async fn stages(db: &SqlitePool, id: i64) -> Result<Vec<StageRun>> {
    Ok(sqlx::query_as(
        "SELECT attempt, stage, outcome, started_at, finished_at FROM job_stages \
         WHERE job_id = ? ORDER BY id",
    )
    .bind(id)
    .fetch_all(db)
    .await?)
}

pub async fn events(db: &SqlitePool, id: i64) -> Result<Vec<Event>> {
    Ok(sqlx::query_as(
        "SELECT * FROM (SELECT * FROM job_events WHERE job_id = ? ORDER BY id DESC LIMIT 500) \
         ORDER BY id",
    )
    .bind(id)
    .fetch_all(db)
    .await?)
}

pub async fn claim_next(db: &SqlitePool) -> Result<Option<Job>> {
    let now = now_ms();
    Ok(sqlx::query_as(
        "UPDATE jobs SET status = 'running', stage = 'queued', progress = NULL, \
         attempts = attempts + 1, error = NULL, error_stage = NULL, started_at = ?, \
         finished_at = NULL, updated_at = ? \
         WHERE id = (SELECT id FROM jobs WHERE status = 'queued' ORDER BY id LIMIT 1) \
         RETURNING *",
    )
    .bind(now)
    .bind(now)
    .fetch_optional(db)
    .await?)
}

/// Jobs left running by a crash or restart go back to the front of the queue.
pub async fn requeue_interrupted(db: &SqlitePool) -> Result<u64> {
    let now = now_ms();
    sqlx::query(
        "UPDATE job_stages SET outcome = 'interrupted', finished_at = ? \
         WHERE outcome = 'running'",
    )
    .bind(now)
    .execute(db)
    .await?;
    Ok(sqlx::query(
        "UPDATE jobs SET status = 'queued', stage = 'queued', progress = NULL, updated_at = ? \
         WHERE status = 'running'",
    )
    .bind(now)
    .execute(db)
    .await?
    .rows_affected())
}

pub async fn start_stage(db: &SqlitePool, id: i64, attempt: i64, stage: Stage) -> Result<()> {
    let now = now_ms();
    sqlx::query(
        "INSERT INTO job_stages (job_id, attempt, stage, outcome, started_at) \
         VALUES (?, ?, ?, 'running', ?)",
    )
    .bind(id)
    .bind(attempt)
    .bind(stage.as_str())
    .bind(now)
    .execute(db)
    .await?;
    sqlx::query("UPDATE jobs SET stage = ?, progress = NULL, updated_at = ? WHERE id = ?")
        .bind(stage.as_str())
        .bind(now)
        .bind(id)
        .execute(db)
        .await?;
    Ok(())
}

pub async fn finish_stage(db: &SqlitePool, id: i64, outcome: &str) -> Result<()> {
    sqlx::query(
        "UPDATE job_stages SET outcome = ?, finished_at = ? \
         WHERE job_id = ? AND outcome = 'running'",
    )
    .bind(outcome)
    .bind(now_ms())
    .bind(id)
    .execute(db)
    .await?;
    Ok(())
}

pub async fn set_progress(db: &SqlitePool, id: i64, progress: Option<f64>) -> Result<()> {
    sqlx::query("UPDATE jobs SET progress = ?, updated_at = ? WHERE id = ?")
        .bind(progress)
        .bind(now_ms())
        .bind(id)
        .execute(db)
        .await?;
    Ok(())
}

pub struct Metadata<'a> {
    pub title: Option<&'a str>,
    pub platform: Option<&'a str>,
    pub uploader: Option<&'a str>,
    pub thumbnail: Option<&'a str>,
    pub duration_secs: Option<f64>,
    pub description: Option<&'a str>,
    pub media_kind: &'a str,
    pub images: &'a [String],
}

pub async fn set_metadata(db: &SqlitePool, id: i64, m: &Metadata<'_>) -> Result<()> {
    sqlx::query(
        "UPDATE jobs SET title = ?, platform = ?, uploader = ?, thumbnail = ?, duration_secs = ?, \
         description = ?, media_kind = ?, images = ?, updated_at = ? WHERE id = ?",
    )
    .bind(m.title)
    .bind(m.platform)
    .bind(m.uploader)
    .bind(m.thumbnail)
    .bind(m.duration_secs)
    .bind(m.description)
    .bind(m.media_kind)
    .bind(Json(m.images))
    .bind(now_ms())
    .bind(id)
    .execute(db)
    .await?;
    Ok(())
}

pub async fn set_transcript(db: &SqlitePool, id: i64, transcript: &str) -> Result<()> {
    sqlx::query("UPDATE jobs SET transcript = ?, updated_at = ? WHERE id = ?")
        .bind(transcript)
        .bind(now_ms())
        .bind(id)
        .execute(db)
        .await?;
    Ok(())
}

pub async fn set_recipe(
    db: &SqlitePool,
    id: i64,
    recipe: &serde_json::Value,
    name: &str,
    prompt_tokens: Option<i64>,
    completion_tokens: Option<i64>,
) -> Result<()> {
    sqlx::query(
        "UPDATE jobs SET recipe_json = ?, recipe_name = ?, \
         prompt_tokens = COALESCE(prompt_tokens, 0) + COALESCE(?, 0), \
         completion_tokens = COALESCE(completion_tokens, 0) + COALESCE(?, 0), \
         updated_at = ? WHERE id = ?",
    )
    .bind(Json(recipe))
    .bind(name)
    .bind(prompt_tokens)
    .bind(completion_tokens)
    .bind(now_ms())
    .bind(id)
    .execute(db)
    .await?;
    Ok(())
}

pub async fn set_slug(db: &SqlitePool, id: i64, slug: &str) -> Result<()> {
    sqlx::query("UPDATE jobs SET mealie_slug = ?, updated_at = ? WHERE id = ?")
        .bind(slug)
        .bind(now_ms())
        .bind(id)
        .execute(db)
        .await?;
    Ok(())
}

pub async fn set_recipe_name(db: &SqlitePool, id: i64, name: &str) -> Result<()> {
    sqlx::query("UPDATE jobs SET recipe_name = ?, updated_at = ? WHERE id = ?")
        .bind(name)
        .bind(now_ms())
        .bind(id)
        .execute(db)
        .await?;
    Ok(())
}

pub async fn add_tokens(
    db: &SqlitePool,
    id: i64,
    prompt_tokens: Option<i64>,
    completion_tokens: Option<i64>,
) -> Result<()> {
    sqlx::query(
        "UPDATE jobs SET prompt_tokens = COALESCE(prompt_tokens, 0) + COALESCE(?, 0), \
         completion_tokens = COALESCE(completion_tokens, 0) + COALESCE(?, 0), \
         updated_at = ? WHERE id = ?",
    )
    .bind(prompt_tokens)
    .bind(completion_tokens)
    .bind(now_ms())
    .bind(id)
    .execute(db)
    .await?;
    Ok(())
}

pub async fn finish(
    db: &SqlitePool,
    id: i64,
    status: Status,
    stage: Stage,
    error: Option<(&str, &str)>,
    slug: Option<&str>,
) -> Result<()> {
    let now = now_ms();
    sqlx::query(
        "UPDATE jobs SET status = ?, stage = ?, progress = NULL, error = ?, error_stage = ?, \
         mealie_slug = COALESCE(?, mealie_slug), finished_at = ?, updated_at = ? WHERE id = ?",
    )
    .bind(status.as_str())
    .bind(stage.as_str())
    .bind(error.map(|e| e.1))
    .bind(error.map(|e| e.0))
    .bind(slug)
    .bind(now)
    .bind(now)
    .bind(id)
    .execute(db)
    .await?;
    Ok(())
}

pub async fn cancel_queued(db: &SqlitePool, id: i64) -> Result<bool> {
    let now = now_ms();
    Ok(sqlx::query(
        "UPDATE jobs SET status = 'cancelled', finished_at = ?, updated_at = ? \
         WHERE id = ? AND status = 'queued'",
    )
    .bind(now)
    .bind(now)
    .bind(id)
    .execute(db)
    .await?
    .rows_affected()
        > 0)
}

pub async fn requeue(db: &SqlitePool, id: i64, fresh: bool) -> Result<bool> {
    // Uploads keep what describes the files themselves (set when they arrived).
    let reset = if fresh {
        ", title = CASE WHEN source = 'file' THEN title END, \
         platform = CASE WHEN source = 'file' THEN platform END, \
         media_kind = CASE WHEN source = 'file' THEN media_kind END, \
         uploader = NULL, thumbnail = NULL, \
         duration_secs = NULL, description = NULL, images = NULL, \
         transcript = NULL, recipe_json = NULL, recipe_name = NULL, mealie_slug = NULL"
    } else {
        ""
    };
    Ok(sqlx::query(&format!(
        "UPDATE jobs SET status = 'queued', stage = 'queued', progress = NULL, error = NULL, \
         error_stage = NULL, finished_at = NULL, updated_at = ?{reset} \
         WHERE id = ? AND status IN ('failed', 'cancelled', 'succeeded')"
    ))
    .bind(now_ms())
    .bind(id)
    .execute(db)
    .await?
    .rows_affected()
        > 0)
}

pub async fn delete(db: &SqlitePool, id: i64) -> Result<bool> {
    Ok(
        sqlx::query("DELETE FROM jobs WHERE id = ? AND status != 'running'")
            .bind(id)
            .execute(db)
            .await?
            .rows_affected()
            > 0,
    )
}

pub async fn clear_finished(db: &SqlitePool, status: &str) -> Result<u64> {
    let statuses = match status {
        "succeeded" => "('succeeded')",
        "failed" => "('failed', 'cancelled')",
        _ => "('succeeded', 'failed', 'cancelled')",
    };
    Ok(
        sqlx::query(&format!("DELETE FROM jobs WHERE status IN {statuses}"))
            .execute(db)
            .await?
            .rows_affected(),
    )
}

pub async fn log(
    db: &SqlitePool,
    id: i64,
    level: &str,
    stage: Option<Stage>,
    message: &str,
) -> Result<Event> {
    Ok(sqlx::query_as(
        "INSERT INTO job_events (job_id, at, level, stage, message) VALUES (?, ?, ?, ?, ?) \
         RETURNING *",
    )
    .bind(id)
    .bind(now_ms())
    .bind(level)
    .bind(stage.map(Stage::as_str))
    .bind(message)
    .fetch_one(db)
    .await?)
}

fn new_token() -> String {
    format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    )
}

/// The bearer token for API clients such as an iOS Shortcut, created on first use.
pub async fn api_token(db: &SqlitePool) -> Result<String> {
    sqlx::query("INSERT OR IGNORE INTO settings (key, value) VALUES ('api_token', ?)")
        .bind(new_token())
        .execute(db)
        .await?;
    Ok(
        sqlx::query_scalar("SELECT value FROM settings WHERE key = 'api_token'")
            .fetch_one(db)
            .await?,
    )
}

pub async fn rotate_api_token(db: &SqlitePool) -> Result<String> {
    let token = new_token();
    sqlx::query("INSERT OR REPLACE INTO settings (key, value) VALUES ('api_token', ?)")
        .bind(&token)
        .execute(db)
        .await?;
    Ok(token)
}

#[derive(Debug, Serialize, Default)]
pub struct Stats {
    pub queued: i64,
    pub running: i64,
    pub succeeded_7d: i64,
    pub failed_7d: i64,
    pub total_succeeded: i64,
    pub avg_duration_ms: Option<f64>,
    pub tokens_7d: i64,
    pub stage_avg_ms: Vec<StageAvg>,
    pub failures_by_stage: Vec<StageCount>,
    pub platforms: Vec<StageCount>,
}

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct StageAvg {
    pub stage: String,
    pub avg_ms: f64,
    pub runs: i64,
}

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct StageCount {
    pub name: String,
    pub count: i64,
}

pub async fn stats(db: &SqlitePool) -> Result<Stats> {
    let week_ago = now_ms() - 7 * 24 * 3600 * 1000;
    let (queued, running, succeeded_7d, failed_7d, total_succeeded, tokens_7d, avg_duration_ms) =
        sqlx::query_as(
            "SELECT \
               COALESCE(SUM(status = 'queued'), 0), \
               COALESCE(SUM(status = 'running'), 0), \
               COALESCE(SUM(status = 'succeeded' AND finished_at >= ?1), 0), \
               COALESCE(SUM(status = 'failed' AND finished_at >= ?1), 0), \
               COALESCE(SUM(status = 'succeeded'), 0), \
               COALESCE(SUM(CASE WHEN created_at >= ?1 \
                 THEN COALESCE(prompt_tokens, 0) + COALESCE(completion_tokens, 0) END), 0), \
               AVG(CASE WHEN status = 'succeeded' AND finished_at >= ?1 \
                 THEN finished_at - started_at END) \
             FROM jobs",
        )
        .bind(week_ago)
        .fetch_one(db)
        .await?;
    Ok(Stats {
        queued,
        running,
        succeeded_7d,
        failed_7d,
        total_succeeded,
        tokens_7d,
        avg_duration_ms,
        stage_avg_ms: sqlx::query_as(
            "SELECT stage, AVG(finished_at - started_at) AS avg_ms, COUNT(*) AS runs \
             FROM job_stages WHERE outcome = 'ok' AND started_at >= ? GROUP BY stage",
        )
        .bind(week_ago)
        .fetch_all(db)
        .await?,
        failures_by_stage: sqlx::query_as(
            "SELECT error_stage AS name, COUNT(*) AS count FROM jobs \
             WHERE status = 'failed' AND error_stage IS NOT NULL AND finished_at >= ? \
             GROUP BY error_stage ORDER BY count DESC",
        )
        .bind(week_ago)
        .fetch_all(db)
        .await?,
        platforms: sqlx::query_as(
            "SELECT platform AS name, COUNT(*) AS count FROM jobs \
             WHERE platform IS NOT NULL AND created_at >= ? \
             GROUP BY platform ORDER BY count DESC LIMIT 6",
        )
        .bind(week_ago)
        .fetch_all(db)
        .await?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn claims_in_order_and_only_once() {
        let db = connect_memory().await.unwrap();
        let a = insert_job(&db, "https://a", "social", &[], None)
            .await
            .unwrap();
        let b = insert_job(&db, "https://b", "web", &["x".into()], None)
            .await
            .unwrap();

        let first = claim_next(&db).await.unwrap().unwrap();
        assert_eq!(first.id, a);
        assert_eq!(first.status, "running");
        assert_eq!(first.attempts, 1);
        let second = claim_next(&db).await.unwrap().unwrap();
        assert_eq!(second.id, b);
        assert_eq!(second.tags.0, vec!["x".to_string()]);
        assert_eq!(second.source, "web");
        assert!(claim_next(&db).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn interrupted_jobs_are_requeued() {
        let db = connect_memory().await.unwrap();
        let id = insert_job(&db, "https://a", "social", &[], None)
            .await
            .unwrap();
        claim_next(&db).await.unwrap();
        start_stage(&db, id, 1, Stage::Download).await.unwrap();

        assert_eq!(requeue_interrupted(&db).await.unwrap(), 1);
        let job = get_summary(&db, id).await.unwrap().unwrap();
        assert_eq!(job.status, "queued");
        assert_eq!(stages(&db, id).await.unwrap()[0].outcome, "interrupted");
    }

    #[tokio::test]
    async fn retry_keeps_cached_work_unless_fresh() {
        let db = connect_memory().await.unwrap();
        let id = insert_job(&db, "https://a", "social", &[], None)
            .await
            .unwrap();
        claim_next(&db).await.unwrap();
        set_transcript(&db, id, "hello").await.unwrap();
        finish(
            &db,
            id,
            Status::Failed,
            Stage::Import,
            Some(("import", "boom")),
            None,
        )
        .await
        .unwrap();

        assert!(requeue(&db, id, false).await.unwrap());
        let job = get_full(&db, id).await.unwrap().unwrap();
        assert_eq!(job.transcript.as_deref(), Some("hello"));
        assert!(job.error.is_none());

        claim_next(&db).await.unwrap();
        assert!(
            !requeue(&db, id, true).await.unwrap(),
            "running jobs stay put"
        );
        finish(
            &db,
            id,
            Status::Failed,
            Stage::Import,
            Some(("import", "boom")),
            None,
        )
        .await
        .unwrap();
        assert!(requeue(&db, id, true).await.unwrap());
        assert!(get_full(&db, id)
            .await
            .unwrap()
            .unwrap()
            .transcript
            .is_none());
    }

    #[tokio::test]
    async fn fresh_retry_keeps_upload_kind() {
        let db = connect_memory().await.unwrap();
        let upload = insert_job(&db, "upload:memo.m4a", "file", &[], None)
            .await
            .unwrap();
        let web = insert_job(&db, "https://a", "web", &[], None)
            .await
            .unwrap();
        for id in [upload, web] {
            sqlx::query(
                "UPDATE jobs SET media_kind = 'video', title = 't', status = 'failed' WHERE id = ?",
            )
            .bind(id)
            .execute(&db)
            .await
            .unwrap();
            assert!(requeue(&db, id, true).await.unwrap());
        }
        let upload = get_full(&db, upload).await.unwrap().unwrap();
        assert_eq!(upload.media_kind.as_deref(), Some("video"));
        assert_eq!(upload.title.as_deref(), Some("t"));
        let web = get_full(&db, web).await.unwrap().unwrap();
        assert!(web.media_kind.is_none() && web.title.is_none());
    }

    #[tokio::test]
    async fn api_token_is_stable_until_rotated() {
        let db = connect_memory().await.unwrap();
        let first = api_token(&db).await.unwrap();
        assert_eq!(first.len(), 64);
        assert_eq!(api_token(&db).await.unwrap(), first);
        let rotated = rotate_api_token(&db).await.unwrap();
        assert_ne!(rotated, first);
        assert_eq!(api_token(&db).await.unwrap(), rotated);
    }

    #[tokio::test]
    async fn stats_and_summaries_load() {
        let db = connect_memory().await.unwrap();
        let id = insert_job(&db, "https://a", "social", &[], None)
            .await
            .unwrap();
        claim_next(&db).await.unwrap();
        start_stage(&db, id, 1, Stage::Metadata).await.unwrap();
        finish_stage(&db, id, "ok").await.unwrap();
        finish(&db, id, Status::Succeeded, Stage::Done, None, Some("soup"))
            .await
            .unwrap();

        let s = stats(&db).await.unwrap();
        assert_eq!(s.succeeded_7d, 1);
        assert_eq!(s.stage_avg_ms.len(), 1);
        assert_eq!(list(&db, "succeeded", 10).await.unwrap().len(), 1);
        assert!(find_imported(&db, "https://a").await.unwrap().is_some());
        let json = serde_json::to_value(get_summary(&db, id).await.unwrap()).unwrap();
        assert!(json.get("transcript").is_none());
        assert_eq!(json["tags"], serde_json::json!([]));
    }
}
