use serde::Serialize;
use sqlx::SqlitePool;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};
use tokio::sync::{broadcast, Notify};
use tokio_util::sync::CancellationToken;

use crate::{
    config::Config,
    db::{self, Event, Job},
};

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Update {
    Job { job: Box<Job> },
    Log { event: Event },
    Deleted { id: i64 },
    Refresh,
}

#[derive(Clone)]
pub struct AppState(Arc<Inner>);

pub struct Inner {
    pub config: Config,
    pub db: SqlitePool,
    pub http: reqwest::Client,
    pub wake: Notify,
    pub updates: broadcast::Sender<Update>,
    pub running: Mutex<HashMap<i64, CancellationToken>>,
}

impl std::ops::Deref for AppState {
    type Target = Inner;

    fn deref(&self) -> &Inner {
        &self.0
    }
}

impl AppState {
    pub fn new(config: Config, db: SqlitePool) -> anyhow::Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent(concat!("mealie-forager/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(std::time::Duration::from_secs(15))
            .timeout(std::time::Duration::from_secs(120))
            .build()?;
        Ok(Self(Arc::new(Inner {
            config,
            db,
            http,
            wake: Notify::new(),
            updates: broadcast::channel(256).0,
            running: Mutex::new(HashMap::new()),
        })))
    }

    pub async fn publish_job(&self, id: i64) {
        if let Ok(Some(job)) = db::get_summary(&self.db, id).await {
            let _ = self.updates.send(Update::Job { job: Box::new(job) });
        }
    }

    pub fn publish_event(&self, event: Event) {
        let _ = self.updates.send(Update::Log { event });
    }
}
