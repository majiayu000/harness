use super::store_tests::open_test_store;
use super::*;
use harness_core::run_id::RunId;
use std::str::FromStr;

fn make_event(hook: &str, decision: Decision) -> Event {
    Event::new(SessionId::new(), hook, "Edit", decision)
}

#[tokio::test(flavor = "multi_thread")]
async fn query_deserializes_rows_without_metadata() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let Some(store) = open_test_store(dir.path()).await? else {
        return Ok(());
    };

    sqlx::query(
        "INSERT INTO events
            (store_key, id, ts, session_id, hook, tool, decision, reason, detail, duration_ms, content, metadata)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)",
    )
    .bind(store.store_key())
    .bind("legacy-event")
    .bind(chrono::Utc::now())
    .bind(SessionId::new().as_str())
    .bind("legacy_hook")
    .bind("legacy_tool")
    .bind("pass")
    .bind(Option::<String>::None)
    .bind(Option::<String>::None)
    .bind(Option::<i64>::None)
    .bind(Option::<String>::None)
    .bind(Option::<String>::None)
    .execute(&store.pool)
    .await?;

    let results = store.query(&EventFilters::default()).await?;
    assert_eq!(results.len(), 1);
    assert!(results[0].metadata.is_none());
    store.close().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn events_table_uses_timestamptz_for_timestamp_ordering() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let Some(store) = open_test_store(dir.path()).await? else {
        return Ok(());
    };

    let row: (String,) = sqlx::query_as(
        "SELECT data_type
         FROM information_schema.columns
         WHERE table_schema = current_schema()
           AND table_name = 'events'
           AND column_name = 'ts'",
    )
    .fetch_one(&store.pool)
    .await?;

    assert_eq!(row.0, "timestamp with time zone");
    store.close().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn raw_decision_column_uses_typed_label_without_json_quotes() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let Some(store) = open_test_store(dir.path()).await? else {
        return Ok(());
    };

    let event = make_event("decision_raw", Decision::Gate);
    store.log(&event).await?;
    let row: (String,) =
        sqlx::query_as("SELECT decision FROM events WHERE store_key = $1 AND id = $2")
            .bind(store.store_key())
            .bind(event.id.as_str())
            .fetch_one(&store.pool)
            .await?;

    assert_eq!(row.0, "gate");
    store.close().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn log_many_persists_events_in_one_batch_path() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let Some(store) = open_test_store(dir.path()).await? else {
        return Ok(());
    };
    let events = vec![
        make_event("batch", Decision::Pass),
        make_event("batch", Decision::Warn),
        make_event("batch", Decision::Block),
    ];

    let ids = store.log_many(&events).await?;
    let results = store
        .query(&EventFilters {
            hook: Some("batch".to_string()),
            ..Default::default()
        })
        .await?;

    assert_eq!(ids.len(), 3);
    assert_eq!(results.len(), 3);
    store.close().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn query_rejects_unrepresentable_limit_before_sql_execution() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let Some(store) = open_test_store(dir.path()).await? else {
        return Ok(());
    };

    let error = store
        .query(&EventFilters {
            limit: Some(usize::MAX),
            ..Default::default()
        })
        .await
        .expect_err("limit outside i64 should be rejected before querying Postgres");

    assert!(
        error
            .to_string()
            .contains("event query limit exceeds i64::MAX"),
        "unexpected error: {error:#}"
    );
    store.close().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn policy_events_for_agent_run_filters_only_run_id() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let Some(store) = open_test_store(dir.path()).await? else {
        return Ok(());
    };
    let run_id = RunId::from_str("ar-01j1qb3c9r7v5m2k8x4tznq6wd")?;
    let mut matching = make_event("policy", Decision::Pass);
    matching.run_id = Some(run_id.clone());
    matching.tool = "codex".to_string();
    let mut same_agent_other_tool = make_event("policy", Decision::Pass);
    same_agent_other_tool.run_id = Some(run_id.clone());
    same_agent_other_tool.tool = "rule_id".to_string();
    let mut other_run = make_event("policy", Decision::Pass);
    other_run.run_id = Some(RunId::from_str("ar-01j1qb3c9r7v5m2k8x4tznq6we")?);
    store
        .log_many(&[matching, same_agent_other_tool, other_run])
        .await?;

    let events = store.policy_events_for_agent_run(&run_id).await?;

    assert_eq!(events.len(), 2);
    assert!(events
        .iter()
        .all(|event| event.run_id.as_ref() == Some(&run_id)));
    store.close().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn migrate_from_jsonl_keeps_source_on_invalid_record() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let valid = make_event("legacy_jsonl", Decision::Warn);
    let jsonl_path = dir.path().join("events.jsonl");
    std::fs::write(
        &jsonl_path,
        format!("{}\n{{not-json\n", serde_json::to_string(&valid)?),
    )?;

    let Some(store) = open_test_store(dir.path()).await? else {
        return Ok(());
    };

    assert!(
        jsonl_path.exists(),
        "invalid JSONL should stay in place for retry"
    );
    assert!(
        !dir.path().join("events.jsonl.migrated").exists(),
        "invalid JSONL must not be archived"
    );
    let results = store.query(&EventFilters::default()).await?;
    assert!(
        results.is_empty(),
        "valid prefix should not be committed when migration aborts"
    );
    store.close().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn purge_old_events_zero_days_is_noop() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let Some(store) = open_test_store(dir.path()).await? else {
        return Ok(());
    };
    store.log(&make_event("hook", Decision::Pass)).await?;
    let deleted = store.purge_old_events(0).await?;
    assert_eq!(deleted, 0);
    let results = store.query(&EventFilters::default()).await?;
    assert_eq!(results.len(), 1);
    store.close().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn purge_old_events_removes_stale_and_keeps_recent() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let Some(store) = open_test_store(dir.path()).await? else {
        return Ok(());
    };

    let mut old_event = make_event("hook", Decision::Pass);
    old_event.ts = chrono::Utc::now() - chrono::Duration::days(101);
    store.log(&old_event).await?;

    let recent_event = make_event("hook", Decision::Pass);
    store.log(&recent_event).await?;

    let deleted = store.purge_old_events(90).await?;
    assert_eq!(deleted, 1, "only the old event should be purged");

    let results = store.query(&EventFilters::default()).await?;
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].id, recent_event.id);
    store.close().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn purge_spares_periodic_review_watermarks() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let Some(store) = open_test_store(dir.path()).await? else {
        return Ok(());
    };

    let mut old_regular = make_event("pre_tool_use", Decision::Pass);
    old_regular.ts = chrono::Utc::now() - chrono::Duration::days(101);
    store.log(&old_regular).await?;

    let mut old_watermark = make_event("periodic_review:my-project", Decision::Pass);
    old_watermark.ts = chrono::Utc::now() - chrono::Duration::days(101);
    store.log(&old_watermark).await?;

    let deleted = store.purge_old_events(90).await?;
    assert_eq!(deleted, 1, "only the regular old event should be purged");

    let results = store.query(&EventFilters::default()).await?;
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].id, old_watermark.id);
    store.close().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn purge_trims_old_watermarks_keeps_newest() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let Some(store) = open_test_store(dir.path()).await? else {
        return Ok(());
    };

    let hook = "periodic_review:my-project";

    let mut wm_old = make_event(hook, Decision::Pass);
    wm_old.ts = chrono::Utc::now() - chrono::Duration::days(200);
    store.log(&wm_old).await?;

    let mut wm_mid = make_event(hook, Decision::Pass);
    wm_mid.ts = chrono::Utc::now() - chrono::Duration::days(100);
    store.log(&wm_mid).await?;

    let mut wm_new = make_event(hook, Decision::Pass);
    wm_new.ts = chrono::Utc::now() - chrono::Duration::days(1);
    store.log(&wm_new).await?;

    let deleted = store.purge_old_events(90).await?;
    assert_eq!(deleted, 2, "two older watermarks should be trimmed");

    let results = store.query(&EventFilters::default()).await?;
    assert_eq!(results.len(), 1, "only the newest watermark should remain");
    assert_eq!(results[0].id, wm_new.id);
    store.close().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn watermark_returns_none_before_first_set() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let Some(store) = open_test_store(dir.path()).await? else {
        return Ok(());
    };
    let result = store.get_scan_watermark("proj", "gc").await?;
    assert!(result.is_none());
    store.close().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn set_then_get_watermark_roundtrip() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let Some(store) = open_test_store(dir.path()).await? else {
        return Ok(());
    };
    let ts = chrono::Utc::now();
    store.set_scan_watermark("proj", "gc", ts).await?;
    let Some(retrieved) = store.get_scan_watermark("proj", "gc").await? else {
        anyhow::bail!("watermark must exist after set");
    };
    assert_eq!(retrieved.timestamp(), ts.timestamp());
    store.close().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn watermarks_are_per_project_and_agent() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let Some(store) = open_test_store(dir.path()).await? else {
        return Ok(());
    };
    let ts = chrono::Utc::now();
    store.set_scan_watermark("proj1", "gc", ts).await?;
    let r1 = store.get_scan_watermark("proj1", "other").await?;
    assert!(r1.is_none(), "different agent_id must not share watermark");
    let r2 = store.get_scan_watermark("proj2", "gc").await?;
    assert!(r2.is_none(), "different project must not share watermark");
    store.close().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn set_watermark_overwrites_previous() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let Some(store) = open_test_store(dir.path()).await? else {
        return Ok(());
    };
    let ts1 = chrono::Utc::now() - chrono::Duration::hours(1);
    let ts2 = chrono::Utc::now();
    store.set_scan_watermark("proj", "gc", ts1).await?;
    store.set_scan_watermark("proj", "gc", ts2).await?;
    let Some(retrieved) = store.get_scan_watermark("proj", "gc").await? else {
        anyhow::bail!("watermark must exist");
    };
    assert_eq!(retrieved.timestamp(), ts2.timestamp());
    store.close().await;
    Ok(())
}
