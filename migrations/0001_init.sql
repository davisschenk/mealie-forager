CREATE TABLE jobs (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    url TEXT NOT NULL,
    tags TEXT NOT NULL DEFAULT '[]',
    note TEXT,
    status TEXT NOT NULL,
    stage TEXT NOT NULL,
    progress REAL,
    attempts INTEGER NOT NULL DEFAULT 0,
    error TEXT,
    error_stage TEXT,
    title TEXT,
    platform TEXT,
    uploader TEXT,
    thumbnail TEXT,
    duration_secs REAL,
    description TEXT,
    media_kind TEXT,
    images TEXT,
    transcript TEXT,
    recipe_json TEXT,
    recipe_name TEXT,
    mealie_slug TEXT,
    prompt_tokens INTEGER,
    completion_tokens INTEGER,
    created_at INTEGER NOT NULL,
    started_at INTEGER,
    finished_at INTEGER,
    updated_at INTEGER NOT NULL
);

CREATE INDEX jobs_status_idx ON jobs (status, id);
CREATE INDEX jobs_url_idx ON jobs (url);

CREATE TABLE job_stages (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    job_id INTEGER NOT NULL REFERENCES jobs (id) ON DELETE CASCADE,
    attempt INTEGER NOT NULL,
    stage TEXT NOT NULL,
    outcome TEXT NOT NULL,
    started_at INTEGER NOT NULL,
    finished_at INTEGER
);

CREATE INDEX job_stages_job_idx ON job_stages (job_id, id);

CREATE TABLE job_events (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    job_id INTEGER NOT NULL REFERENCES jobs (id) ON DELETE CASCADE,
    at INTEGER NOT NULL,
    level TEXT NOT NULL,
    stage TEXT,
    message TEXT NOT NULL
);

CREATE INDEX job_events_job_idx ON job_events (job_id, id);
