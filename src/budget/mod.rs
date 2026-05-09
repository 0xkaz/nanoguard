pub mod sqlite;
pub mod store;

pub use sqlite::SqliteBudgetStore;
pub use store::BudgetStore;

use crate::config::BudgetConfig;
use anyhow::Result;
use std::sync::Arc;

pub async fn build(cfg: &BudgetConfig) -> Result<Arc<dyn BudgetStore>> {
    let store = SqliteBudgetStore::open(&cfg.db_path).await?;
    Ok(Arc::new(store))
}
