mod api;
mod clean;
mod config;
mod db;
mod mealie;
mod media;
mod openai;
mod state;
mod uploads;
mod urls;
mod web;
mod worker;

use anyhow::Result;
use tracing::info;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let config = config::Config::from_env()?;
    tokio::fs::create_dir_all(&config.work_dir).await?;
    let pool = db::connect(&config.database_path).await?;
    let requeued = db::requeue_interrupted(&pool).await?;
    if requeued > 0 {
        info!("requeued {requeued} interrupted job(s)");
    }

    let listen = config.listen;
    let workers = config.workers;
    let api_token = db::api_token(&pool).await?;
    tokio::fs::create_dir_all(&config.upload_dir).await?;
    uploads::prune(&pool, &config.upload_dir).await?;
    let state = state::AppState::new(config, pool, api_token)?;
    for i in 0..workers {
        tokio::spawn(worker::run(state.clone(), i));
    }

    let app = api::router(&state)
        .merge(web::router())
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            api::auth,
        ))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind(listen).await?;
    info!("listening on http://{listen}");
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    // Exit without draining: SSE clients never disconnect on their own, and
    // interrupted jobs are requeued on the next start.
    tokio::select! {
        r = axum::serve(listener, app) => r?,
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
    }
    info!("shutting down");
    Ok(())
}
