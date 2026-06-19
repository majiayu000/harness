use super::*;
use futures::FutureExt;
use harness_core::db::{pg_open_pool, resolve_database_url, PgStoreContext};
use tempfile::tempdir;

fn unique_test_schema(prefix: &str) -> String {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let count = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock before UNIX epoch")
        .as_nanos();
    format!("{prefix}_{nanos}_{count}")
}

async fn open_test_store() -> anyhow::Result<Option<(QValueStore, tempfile::TempDir)>> {
    if resolve_database_url(None).is_err() {
        return Ok(None);
    }
    let dir = tempdir()?;
    let path = dir.path().join("q_values.db");
    let store = QValueStore::open(&path).await?;
    Ok(Some((store, dir)))
}

#[test]
fn shared_schema_context_uses_fixed_q_value_store_schema() -> anyhow::Result<()> {
    let context =
        QValueStore::shared_schema_context(Some("postgres://user:pass@localhost:5432/harness"))?;
    assert_eq!(context.schema(), Q_VALUE_STORE_SCHEMA);
    assert!(
        context.ownership().is_none(),
        "shared q_value_store schema must not register path-derived ownership"
    );
    Ok(())
}

#[tokio::test]
async fn shared_schema_q_value_store_keeps_store_rows_isolated() -> anyhow::Result<()> {
    let database_url = match resolve_database_url(None) {
        Ok(url) => url,
        Err(_) => return Ok(()),
    };
    let dir = tempdir()?;
    let setup_pool = pg_open_pool(&database_url).await?;
    let shared_schema = unique_test_schema("q_value_store_scope_test");
    let shared_context = PgStoreContext::from_schema(&shared_schema, Some(&database_url))?;
    let store_a_dir = dir.path().join("store-a");
    let store_b_dir = dir.path().join("store-b");
    std::fs::create_dir_all(&store_a_dir)?;
    std::fs::create_dir_all(&store_b_dir)?;
    let store_a =
        QValueStore::open_shared_with_data_dir(&shared_context, &setup_pool, &store_a_dir).await?;
    let store_b =
        QValueStore::open_shared_with_data_dir(&shared_context, &setup_pool, &store_b_dir).await?;

    let result = std::panic::AssertUnwindSafe(async {
        assert_ne!(store_a.store_key(), store_b.store_key());

        store_a
            .record_pipeline_event("same-task", "plan", &["rule-A"])
            .await?;
        store_b
            .record_pipeline_event("same-task", "plan", &["rule-B"])
            .await?;

        assert_eq!(
            store_a.get_experiences_for_task("same-task").await?,
            vec!["rule-A"]
        );
        assert_eq!(
            store_b.get_experiences_for_task("same-task").await?,
            vec!["rule-B"]
        );

        let shared_rule = vec!["shared-rule".to_string()];
        store_a
            .apply_q_update(&shared_rule, REWARD_MERGED, DEFAULT_ALPHA)
            .await?;
        store_b
            .apply_q_update(&shared_rule, REWARD_CLOSED, DEFAULT_ALPHA)
            .await?;

        let q_a = store_a
            .q_value_for("shared-rule")
            .await?
            .ok_or_else(|| anyhow::anyhow!("store A shared-rule row missing"))?;
        let q_b = store_b
            .q_value_for("shared-rule")
            .await?
            .ok_or_else(|| anyhow::anyhow!("store B shared-rule row missing"))?;
        assert!(
            (q_a - 0.55).abs() < 1e-9,
            "expected store A ~0.55, got {q_a}"
        );
        assert!(
            (q_b - 0.45).abs() < 1e-9,
            "expected store B ~0.45, got {q_b}"
        );

        Ok::<(), anyhow::Error>(())
    })
    .catch_unwind()
    .await;

    store_a.pool.close().await;
    store_b.pool.close().await;
    let _ = sqlx::query(&format!(
        "DROP SCHEMA IF EXISTS \"{shared_schema}\" CASCADE"
    ))
    .execute(&setup_pool)
    .await;
    setup_pool.close().await;

    match result {
        Ok(result) => result,
        Err(payload) => std::panic::resume_unwind(payload),
    }
}

#[tokio::test]
async fn legacy_q_value_store_migration_backfills_once() -> anyhow::Result<()> {
    let database_url = match resolve_database_url(None) {
        Ok(url) => url,
        Err(_) => return Ok(()),
    };
    let dir = tempdir()?;
    let target_data_dir = dir.path().join("target-data");
    let other_data_dir = dir.path().join("other-data");
    std::fs::create_dir_all(&target_data_dir)?;
    std::fs::create_dir_all(&other_data_dir)?;
    let legacy_path = target_data_dir.join("q_values.db");
    let legacy_schema = PgStoreContext::from_legacy_path_schema(&legacy_path, Some(&database_url))?
        .schema()
        .to_owned();
    let target_schema = unique_test_schema("q_value_store_migration_test");
    let setup_pool = pg_open_pool(&database_url).await?;
    let target_context = PgStoreContext::from_schema(&target_schema, Some(&database_url))?;
    let target_store =
        QValueStore::open_shared_with_data_dir(&target_context, &setup_pool, &target_data_dir)
            .await?;
    let other_store =
        QValueStore::open_shared_with_data_dir(&target_context, &setup_pool, &other_data_dir)
            .await?;
    let legacy_store =
        QValueStore::open_with_database_url(&legacy_path, Some(&database_url)).await?;

    let result = std::panic::AssertUnwindSafe(async {
        legacy_store
            .record_pipeline_event("legacy-task", "implement", &["legacy-rule"])
            .await?;
        let experiences = legacy_store.get_experiences_for_task("legacy-task").await?;
        legacy_store
            .apply_q_update(&experiences, REWARD_MERGED, DEFAULT_ALPHA)
            .await?;

        let copied = migrate_legacy_q_value_store_if_needed(
            &legacy_path,
            Some(&database_url),
            &target_store,
        )
        .await?;
        assert_eq!(copied, 2, "one event and one rule row should be copied");

        let copied_again = migrate_legacy_q_value_store_if_needed(
            &legacy_path,
            Some(&database_url),
            &target_store,
        )
        .await?;
        assert_eq!(copied_again, 0, "migration must be idempotent");

        assert_eq!(
            target_store.get_experiences_for_task("legacy-task").await?,
            vec!["legacy-rule"]
        );
        let q = target_store
            .q_value_for("legacy-rule")
            .await?
            .ok_or_else(|| anyhow::anyhow!("legacy-rule row missing after backfill"))?;
        assert!(
            (q - 0.55).abs() < 1e-9,
            "expected migrated q ~0.55, got {q}"
        );
        assert!(
            other_store
                .get_experiences_for_task("legacy-task")
                .await?
                .is_empty(),
            "other data_dir scopes must not hydrate legacy q-value events"
        );
        assert!(
            other_store.q_value_for("legacy-rule").await?.is_none(),
            "other data_dir scopes must not hydrate legacy rule q-values"
        );

        Ok::<(), anyhow::Error>(())
    })
    .catch_unwind()
    .await;

    legacy_store.pool.close().await;
    target_store.pool.close().await;
    other_store.pool.close().await;
    let _ = sqlx::query(&format!(
        "DROP SCHEMA IF EXISTS \"{legacy_schema}\" CASCADE"
    ))
    .execute(&setup_pool)
    .await;
    let _ = sqlx::query(&format!(
        "DROP SCHEMA IF EXISTS \"{target_schema}\" CASCADE"
    ))
    .execute(&setup_pool)
    .await;
    setup_pool.close().await;

    match result {
        Ok(result) => result,
        Err(payload) => std::panic::resume_unwind(payload),
    }
}

#[test]
fn store_key_for_missing_data_dir_errors() -> anyhow::Result<()> {
    let dir = tempdir()?;
    let missing = dir.path().join("missing-data-dir");
    let err = QValueStore::store_key_for_data_dir(&missing)
        .expect_err("missing data dir should not produce a fallback store key");
    let msg = err.to_string();
    assert!(
        msg.contains("failed to canonicalize q_value_store data_dir"),
        "error should mention q_value_store data_dir canonicalization, got: {msg}"
    );
    Ok(())
}

#[tokio::test]
async fn legacy_q_value_store_migration_ignores_missing_legacy_schema() -> anyhow::Result<()> {
    let database_url = match resolve_database_url(None) {
        Ok(url) => url,
        Err(_) => return Ok(()),
    };
    let dir = tempdir()?;
    let target_data_dir = dir.path().join("target-data");
    std::fs::create_dir_all(&target_data_dir)?;
    let missing_legacy_path = dir.path().join("missing-legacy").join("q_values.db");
    let target_schema = unique_test_schema("q_value_store_missing_legacy_test");
    let setup_pool = pg_open_pool(&database_url).await?;
    let target_context = PgStoreContext::from_schema(&target_schema, Some(&database_url))?;
    let target_store =
        QValueStore::open_shared_with_data_dir(&target_context, &setup_pool, &target_data_dir)
            .await?;

    let result = std::panic::AssertUnwindSafe(async {
        let copied = migrate_legacy_q_value_store_if_needed(
            &missing_legacy_path,
            Some(&database_url),
            &target_store,
        )
        .await?;
        assert_eq!(copied, 0, "missing legacy schema should be a no-op");
        Ok::<(), anyhow::Error>(())
    })
    .catch_unwind()
    .await;

    target_store.pool.close().await;
    let _ = sqlx::query(&format!(
        "DROP SCHEMA IF EXISTS \"{target_schema}\" CASCADE"
    ))
    .execute(&setup_pool)
    .await;
    setup_pool.close().await;

    match result {
        Ok(result) => result,
        Err(payload) => std::panic::resume_unwind(payload),
    }
}

#[tokio::test]
async fn record_and_retrieve_pipeline_event() -> anyhow::Result<()> {
    let Some((store, _dir)) = open_test_store().await? else {
        return Ok(());
    };
    store
        .record_pipeline_event("task-1", "implement", &["rule-A", "rule-B"])
        .await?;
    let ids = store.get_experiences_for_task("task-1").await?;
    assert_eq!(ids, vec!["rule-A", "rule-B"]);
    Ok(())
}

#[tokio::test]
async fn get_experiences_deduplicates_across_phases() -> anyhow::Result<()> {
    let Some((store, _dir)) = open_test_store().await? else {
        return Ok(());
    };
    store
        .record_pipeline_event("task-2", "triage", &["rule-A"])
        .await?;
    store
        .record_pipeline_event("task-2", "implement", &["rule-A", "rule-B"])
        .await?;
    let ids = store.get_experiences_for_task("task-2").await?;
    assert_eq!(ids, vec!["rule-A", "rule-B"]);
    Ok(())
}

#[tokio::test]
async fn q_value_update_merged_increases_q_value() -> anyhow::Result<()> {
    let Some((store, _dir)) = open_test_store().await? else {
        return Ok(());
    };
    store
        .record_pipeline_event("task-3", "implement", &["rule-X"])
        .await?;
    let experiences = store.get_experiences_for_task("task-3").await?;
    store
        .apply_q_update(&experiences, REWARD_MERGED, DEFAULT_ALPHA)
        .await?;
    let q = store
        .q_value_for("rule-X")
        .await?
        .ok_or_else(|| anyhow::anyhow!("rule-X row missing"))?;
    assert!((q - 0.55).abs() < 1e-9, "expected ~0.55, got {q}");
    Ok(())
}

#[tokio::test]
async fn q_value_update_closed_decreases_q_value() -> anyhow::Result<()> {
    let Some((store, _dir)) = open_test_store().await? else {
        return Ok(());
    };
    store
        .record_pipeline_event("task-4", "implement", &["rule-Y"])
        .await?;
    let experiences = store.get_experiences_for_task("task-4").await?;
    store
        .apply_q_update(&experiences, REWARD_CLOSED, DEFAULT_ALPHA)
        .await?;
    let q = store
        .q_value_for("rule-Y")
        .await?
        .ok_or_else(|| anyhow::anyhow!("rule-Y row missing"))?;
    assert!((q - 0.45).abs() < 1e-9, "expected ~0.45, got {q}");
    Ok(())
}

#[tokio::test]
async fn q_value_update_noop_for_empty_ids() -> anyhow::Result<()> {
    let Some((store, _dir)) = open_test_store().await? else {
        return Ok(());
    };
    store
        .apply_q_update(&[], REWARD_MERGED, DEFAULT_ALPHA)
        .await?;
    let q = store.q_value_for("nonexistent").await?;
    assert!(q.is_none());
    Ok(())
}

#[tokio::test]
async fn retrieval_count_incremented_via_record_pipeline_event() -> anyhow::Result<()> {
    let Some((store, _dir)) = open_test_store().await? else {
        return Ok(());
    };
    store
        .record_pipeline_event("task-5", "plan", &["rule-Z"])
        .await?;
    store
        .record_pipeline_event("task-5", "implement", &["rule-Z"])
        .await?;
    let row: Option<(i64,)> =
        sqlx::query_as("SELECT retrieval_count FROM rule_experiences WHERE rule_id = $1")
            .bind("rule-Z")
            .fetch_optional(&store.pool)
            .await?;
    assert_eq!(
        row.ok_or_else(|| anyhow::anyhow!("rule-Z row missing"))?.0,
        2
    );
    Ok(())
}

#[tokio::test]
async fn success_count_incremented_only_on_merged_reward() -> anyhow::Result<()> {
    let Some((store, _dir)) = open_test_store().await? else {
        return Ok(());
    };
    store
        .record_pipeline_event("task-6", "implement", &["rule-W"])
        .await?;
    let experiences = store.get_experiences_for_task("task-6").await?;

    store
        .apply_q_update(&experiences, REWARD_CLOSED, DEFAULT_ALPHA)
        .await?;
    let row: Option<(i64,)> =
        sqlx::query_as("SELECT success_count FROM rule_experiences WHERE rule_id = $1")
            .bind("rule-W")
            .fetch_optional(&store.pool)
            .await?;
    assert_eq!(
        row.ok_or_else(|| anyhow::anyhow!("rule-W row missing after closed"))?
            .0,
        0
    );

    store
        .apply_q_update(&experiences, REWARD_MERGED, DEFAULT_ALPHA)
        .await?;
    let row: Option<(i64,)> =
        sqlx::query_as("SELECT success_count FROM rule_experiences WHERE rule_id = $1")
            .bind("rule-W")
            .fetch_optional(&store.pool)
            .await?;
    assert_eq!(
        row.ok_or_else(|| anyhow::anyhow!("rule-W row missing after merged"))?
            .0,
        1
    );
    Ok(())
}
