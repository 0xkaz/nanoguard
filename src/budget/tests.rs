#[cfg(test)]
mod tests {
    use crate::budget::store::{BudgetCheck, SpendRecord};
    use crate::budget::{BudgetStore, SqliteBudgetStore};
    use chrono::Utc;

    async fn test_store() -> SqliteBudgetStore {
        // Use in-memory SQLite for tests
        SqliteBudgetStore::open(":memory:")
            .await
            .expect("open :memory:")
    }

    #[tokio::test]
    async fn new_key_has_zero_usage() {
        let store = test_store().await;
        let usage = store.get_usage("test-key").await.unwrap();
        assert_eq!(usage, 0);
    }

    #[tokio::test]
    async fn no_limit_means_unlimited() {
        let store = test_store().await;
        let limit = store.get_limit("test-key").await.unwrap();
        assert!(limit.is_none());
    }

    #[tokio::test]
    async fn unlimited_key_passes_check() {
        let store = test_store().await;
        let check = store.check("test-key").await.unwrap();
        assert!(matches!(check, BudgetCheck::Unlimited));
    }

    #[tokio::test]
    async fn record_spend_accumulates() {
        let store = test_store().await;
        let record = SpendRecord {
            api_key: "key-a".to_string(),
            prompt_tokens: 100,
            completion_tokens: 50,
            model: "qwen3:0.6b".to_string(),
            created_at: Utc::now(),
        };
        store.record_spend(&record).await.unwrap();
        store.record_spend(&record).await.unwrap();

        let usage = store.get_usage("key-a").await.unwrap();
        assert_eq!(usage, 300); // (100+50) * 2
    }

    #[tokio::test]
    async fn reset_usage_zeroes_counter() {
        let store = test_store().await;
        let record = SpendRecord {
            api_key: "key-b".to_string(),
            prompt_tokens: 200,
            completion_tokens: 100,
            model: "test".to_string(),
            created_at: Utc::now(),
        };
        store.record_spend(&record).await.unwrap();
        assert_eq!(store.get_usage("key-b").await.unwrap(), 300);

        store.reset_usage("key-b").await.unwrap();
        assert_eq!(store.get_usage("key-b").await.unwrap(), 0);
    }

    #[tokio::test]
    async fn budget_ok_when_under_limit() {
        let store = test_store().await;
        store.set_limit("key-limited", 1000).await.unwrap();

        let record = SpendRecord {
            api_key: "key-limited".to_string(),
            prompt_tokens: 300,
            completion_tokens: 100,
            model: "test".to_string(),
            created_at: Utc::now(),
        };
        store.record_spend(&record).await.unwrap();

        let check = store.check("key-limited").await.unwrap();
        assert!(matches!(
            check,
            BudgetCheck::Ok {
                usage: 400,
                limit: 1000
            }
        ));
    }

    #[tokio::test]
    async fn budget_exceeded_when_over_limit() {
        let store = test_store().await;
        store.set_limit("key-exceeded", 100).await.unwrap();

        let record = SpendRecord {
            api_key: "key-exceeded".to_string(),
            prompt_tokens: 80,
            completion_tokens: 40,
            model: "test".to_string(),
            created_at: Utc::now(),
        };
        store.record_spend(&record).await.unwrap(); // total = 120 > 100

        let check = store.check("key-exceeded").await.unwrap();
        assert!(matches!(
            check,
            BudgetCheck::Exceeded {
                usage: 120,
                limit: 100
            }
        ));
    }

    #[tokio::test]
    async fn set_limit_updates_existing_limit() {
        let store = test_store().await;
        store.set_limit("key-update", 500).await.unwrap();
        assert_eq!(store.get_limit("key-update").await.unwrap(), Some(500));

        store.set_limit("key-update", 2000).await.unwrap();
        assert_eq!(store.get_limit("key-update").await.unwrap(), Some(2000));
    }

    #[tokio::test]
    async fn different_keys_are_independent() {
        let store = test_store().await;
        let make_record = |key: &str| SpendRecord {
            api_key: key.to_string(),
            prompt_tokens: 10,
            completion_tokens: 5,
            model: "test".to_string(),
            created_at: Utc::now(),
        };
        store.record_spend(&make_record("key-x")).await.unwrap();
        store.record_spend(&make_record("key-x")).await.unwrap();
        store.record_spend(&make_record("key-y")).await.unwrap();

        assert_eq!(store.get_usage("key-x").await.unwrap(), 30);
        assert_eq!(store.get_usage("key-y").await.unwrap(), 15);
        assert_eq!(store.get_usage("key-z").await.unwrap(), 0);
    }
}
