use anyhow::Result;
use async_trait::async_trait;
use chrono::Utc;
use rusqlite::{params, OptionalExtension};
use std::sync::{Arc, Mutex};
use tokio::task::spawn_blocking;

use super::store::{BudgetStore, SpendRecord};

pub struct SqliteBudgetStore {
    conn: Arc<Mutex<rusqlite::Connection>>,
}

impl SqliteBudgetStore {
    pub async fn open(path: &str) -> Result<Self> {
        let path = path.to_string();
        let conn = spawn_blocking(move || -> Result<rusqlite::Connection> {
            let conn = rusqlite::Connection::open(&path)?;
            conn.execute_batch(SCHEMA)?;
            Ok(conn)
        })
        .await??;

        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }
}

#[async_trait]
impl BudgetStore for SqliteBudgetStore {
    async fn record_spend(&self, record: &SpendRecord) -> Result<()> {
        let conn = Arc::clone(&self.conn);
        let api_key = record.api_key.clone();
        let prompt = record.prompt_tokens;
        let completion = record.completion_tokens;
        let total = prompt + completion;
        let model = record.model.clone();
        let created_at = record.created_at.to_rfc3339();

        spawn_blocking(move || -> Result<()> {
            let conn = conn.lock().unwrap();
            conn.execute(
                "INSERT INTO spend_logs (api_key, prompt_tokens, completion_tokens, total_tokens, model, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![api_key, prompt, completion, total, model, created_at],
            )?;
            conn.execute(
                "INSERT INTO api_key_usage (api_key, total_tokens)
                 VALUES (?1, ?2)
                 ON CONFLICT(api_key) DO UPDATE SET total_tokens = total_tokens + excluded.total_tokens",
                params![api_key, total],
            )?;
            Ok(())
        })
        .await??;
        Ok(())
    }

    async fn get_usage(&self, api_key: &str) -> Result<u64> {
        let conn = Arc::clone(&self.conn);
        let api_key = api_key.to_string();
        let usage = spawn_blocking(move || -> Result<u64> {
            let conn = conn.lock().unwrap();
            let result: Option<u64> = conn
                .query_row(
                    "SELECT total_tokens FROM api_key_usage WHERE api_key = ?1",
                    params![api_key],
                    |row| row.get(0),
                )
                .optional()?;
            Ok(result.unwrap_or(0))
        })
        .await??;
        Ok(usage)
    }

    async fn get_limit(&self, api_key: &str) -> Result<Option<u64>> {
        let conn = Arc::clone(&self.conn);
        let api_key = api_key.to_string();
        let limit = spawn_blocking(move || -> Result<Option<u64>> {
            let conn = conn.lock().unwrap();
            let result = conn
                .query_row(
                    "SELECT token_limit FROM api_key_limits WHERE api_key = ?1",
                    params![api_key],
                    |row| row.get(0),
                )
                .optional()?;
            Ok(result)
        })
        .await??;
        Ok(limit)
    }

    async fn reset_usage(&self, api_key: &str) -> Result<()> {
        let conn = Arc::clone(&self.conn);
        let api_key = api_key.to_string();
        spawn_blocking(move || -> Result<()> {
            let conn = conn.lock().unwrap();
            conn.execute(
                "UPDATE api_key_usage SET total_tokens = 0, reset_at = ?1 WHERE api_key = ?2",
                params![Utc::now().to_rfc3339(), api_key],
            )?;
            Ok(())
        })
        .await??;
        Ok(())
    }
}

const SCHEMA: &str = "
PRAGMA journal_mode=WAL;

CREATE TABLE IF NOT EXISTS api_key_limits (
    api_key     TEXT PRIMARY KEY,
    token_limit INTEGER NOT NULL,
    created_at  TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE TABLE IF NOT EXISTS api_key_usage (
    api_key       TEXT PRIMARY KEY,
    total_tokens  INTEGER NOT NULL DEFAULT 0,
    reset_at      TEXT
);

CREATE TABLE IF NOT EXISTS spend_logs (
    id                INTEGER PRIMARY KEY AUTOINCREMENT,
    api_key           TEXT NOT NULL,
    prompt_tokens     INTEGER NOT NULL DEFAULT 0,
    completion_tokens INTEGER NOT NULL DEFAULT 0,
    total_tokens      INTEGER NOT NULL DEFAULT 0,
    model             TEXT NOT NULL DEFAULT '',
    created_at        TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_spend_logs_api_key ON spend_logs(api_key);
CREATE INDEX IF NOT EXISTS idx_spend_logs_created_at ON spend_logs(created_at);
";
