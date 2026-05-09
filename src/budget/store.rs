use anyhow::Result;
use async_trait::async_trait;
use chrono::{DateTime, Utc};

#[derive(Debug, Clone)]
pub struct SpendRecord {
    pub api_key: String,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub model: String,
    pub created_at: DateTime<Utc>,
}

/// Pluggable budget storage — implement this to swap SQLite → PostgreSQL/Redis
#[async_trait]
pub trait BudgetStore: Send + Sync {
    /// Record token usage after a successful LLM response.
    async fn record_spend(&self, record: &SpendRecord) -> Result<()>;

    /// Total tokens used by this api_key in the current budget window.
    async fn get_usage(&self, api_key: &str) -> Result<u64>;

    /// Token limit for this api_key (None = unlimited).
    async fn get_limit(&self, api_key: &str) -> Result<Option<u64>>;

    /// Reset usage counter for this api_key (called by budget reset scheduler).
    async fn reset_usage(&self, api_key: &str) -> Result<()>;

    /// Check if api_key is over budget. Returns the limit if exceeded.
    async fn check(&self, api_key: &str) -> Result<BudgetCheck> {
        let limit = self.get_limit(api_key).await?;
        let Some(limit) = limit else {
            return Ok(BudgetCheck::Unlimited);
        };
        let usage = self.get_usage(api_key).await?;
        if usage >= limit {
            Ok(BudgetCheck::Exceeded { usage, limit })
        } else {
            Ok(BudgetCheck::Ok { usage, limit })
        }
    }
}

#[derive(Debug)]
pub enum BudgetCheck {
    Unlimited,
    Ok { usage: u64, limit: u64 },
    Exceeded { usage: u64, limit: u64 },
}
