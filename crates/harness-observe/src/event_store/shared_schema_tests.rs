use super::*;
use harness_core::db::{pg_open_pool, resolve_test_database_url, PgStoreContext, TestSchemaGuard};
use harness_core::types::EventId;
use sqlx::postgres::PgPool;
use std::path::Path;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

static DB_SEMAPHORE: OnceLock<Arc<tokio::sync::Semaphore>> = OnceLock::new();

fn db_semaphore() -> Arc<tokio::sync::Semaphore> {
    DB_SEMAPHORE
        .get_or_init(|| Arc::new(tokio::sync::Semaphore::new(1)))
        .clone()
}

fn is_db_unavailable(err: &anyhow::Error) -> bool {
    let msg = format!("{:#}", err);
    msg.contains("pool timed out while waiting for an open connection")
        || msg.contains("EMAXCONNSESSION")
        || msg.contains("SSLRequest")
        || msg.contains("terminating connection due to administrator command")
}

async fn setup_pool() -> anyhow::Result<Option<(String, PgPool, tokio::sync::OwnedSemaphorePermit)>>
{
    let Ok(database_url) = resolve_test_database_url(None) else {
        return Ok(None);
    };
    let permit = db_semaphore()
        .acquire_owned()
        .await
        .expect("semaphore never closed");
    match tokio::time::timeout(Duration::from_secs(2), pg_open_pool(&database_url)).await {
        Ok(Ok(pool)) => Ok(Some((database_url, pool, permit))),
        Ok(Err(err)) if is_db_unavailable(&err) => Ok(None),
        Ok(Err(err)) => Err(err),
        Err(_) => Ok(None),
    }
}

async fn open_shared_store(
    data_dir: &Path,
    context: &PgStoreContext,
    setup_pool: &PgPool,
) -> anyhow::Result<Option<EventStore>> {
    match EventStore::new_shared_with_context(data_dir, context, setup_pool).await {
        Ok(store) => Ok(Some(store)),
        Err(err) if is_db_unavailable(&err) => Ok(None),
        Err(err) => Err(err),
    }
}

fn make_test_event(id: &str, detail: &str) -> Event {
    let mut event = Event::new(SessionId::new(), "shared_schema", "test", Decision::Pass);
    event.id = EventId::from_str(id);
    event.detail = Some(detail.to_string());
    event
}

#[test]
fn shared_schema_context_uses_fixed_event_store_schema() -> anyhow::Result<()> {
    let context =
        EventStore::shared_schema_context(Some("postgres://user:pass@localhost:5432/harness"))?;
    assert_eq!(context.schema(), EVENT_STORE_SCHEMA);
    assert!(
        context.ownership().is_none(),
        "shared event_store schema must not register path-derived ownership"
    );
    Ok(())
}

#[test]
fn store_key_for_missing_data_dir_errors() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let missing_data_dir = dir.path().join("missing-data-dir");

    let error = EventStore::store_key_for_data_dir(&missing_data_dir)
        .expect_err("missing data_dir should not produce a fallback store key");

    assert!(
        error
            .to_string()
            .contains("failed to canonicalize event store data_dir"),
        "unexpected error: {error:#}"
    );
    Ok(())
}

#[tokio::test]
async fn shared_schema_keeps_events_and_watermarks_isolated() -> anyhow::Result<()> {
    let Some((database_url, setup_pool, _permit)) = setup_pool().await? else {
        return Ok(());
    };
    let dir = tempfile::tempdir()?;
    let mut shared_schema = TestSchemaGuard::new(&database_url, "event_store_scope_test")?;
    let shared_context = PgStoreContext::from_schema(shared_schema.schema(), Some(&database_url))?;
    let store_a_dir = dir.path().join("store-a");
    let store_b_dir = dir.path().join("store-b");

    let Some(store_a) = open_shared_store(&store_a_dir, &shared_context, &setup_pool).await? else {
        setup_pool.close().await;
        return Ok(());
    };
    let Some(store_b) = open_shared_store(&store_b_dir, &shared_context, &setup_pool).await? else {
        store_a.close().await;
        setup_pool.close().await;
        return Ok(());
    };

    assert_eq!(store_a.schema(), shared_schema.schema());
    assert_eq!(store_b.schema(), shared_schema.schema());
    assert_ne!(store_a.store_key(), store_b.store_key());

    store_a
        .log(&make_test_event("same-event-id", "store-a"))
        .await?;
    store_b
        .log(&make_test_event("same-event-id", "store-b"))
        .await?;

    let events_a = store_a.query(&EventFilters::default()).await?;
    let events_b = store_b.query(&EventFilters::default()).await?;
    assert_eq!(events_a.len(), 1);
    assert_eq!(events_b.len(), 1);
    assert_eq!(events_a[0].detail.as_deref(), Some("store-a"));
    assert_eq!(events_b[0].detail.as_deref(), Some("store-b"));

    let ts_a = chrono::Utc::now() - chrono::Duration::minutes(5);
    let ts_b = chrono::Utc::now();
    store_a.set_scan_watermark("project", "gc", ts_a).await?;
    store_b.set_scan_watermark("project", "gc", ts_b).await?;
    assert_eq!(
        store_a
            .get_scan_watermark("project", "gc")
            .await?
            .expect("store a watermark")
            .timestamp(),
        ts_a.timestamp()
    );
    assert_eq!(
        store_b
            .get_scan_watermark("project", "gc")
            .await?
            .expect("store b watermark")
            .timestamp(),
        ts_b.timestamp()
    );

    store_a.close().await;
    store_b.close().await;
    shared_schema.cleanup_with_pool(&setup_pool).await?;
    setup_pool.close().await;
    Ok(())
}

#[tokio::test]
async fn jsonl_migration_is_idempotent_per_store_key() -> anyhow::Result<()> {
    let Some((database_url, setup_pool, _permit)) = setup_pool().await? else {
        return Ok(());
    };
    let dir = tempfile::tempdir()?;
    let mut shared_schema = TestSchemaGuard::new(&database_url, "event_store_jsonl_test")?;
    let shared_context = PgStoreContext::from_schema(shared_schema.schema(), Some(&database_url))?;
    let store_a_dir = dir.path().join("store-a");
    let store_b_dir = dir.path().join("store-b");
    std::fs::create_dir_all(&store_a_dir)?;
    std::fs::create_dir_all(&store_b_dir)?;

    let event_a = make_test_event("jsonl-a", "store-a");
    let event_b = make_test_event("jsonl-b", "store-b");
    std::fs::write(
        store_a_dir.join("events.jsonl"),
        format!("{}\n", serde_json::to_string(&event_a)?),
    )?;
    std::fs::write(
        store_b_dir.join("events.jsonl"),
        format!("{}\n", serde_json::to_string(&event_b)?),
    )?;

    let Some(store_a) = open_shared_store(&store_a_dir, &shared_context, &setup_pool).await? else {
        setup_pool.close().await;
        return Ok(());
    };
    let Some(store_b) = open_shared_store(&store_b_dir, &shared_context, &setup_pool).await? else {
        store_a.close().await;
        setup_pool.close().await;
        return Ok(());
    };

    let events_a = store_a.query(&EventFilters::default()).await?;
    let events_b = store_b.query(&EventFilters::default()).await?;
    assert_eq!(events_a.len(), 1);
    assert_eq!(events_b.len(), 1);
    assert_eq!(events_a[0].id, event_a.id);
    assert_eq!(events_b[0].id, event_b.id);

    store_a.close().await;
    store_b.close().await;
    shared_schema.cleanup_with_pool(&setup_pool).await?;
    setup_pool.close().await;
    Ok(())
}

#[tokio::test]
async fn shared_schema_backfills_legacy_path_schema_once() -> anyhow::Result<()> {
    let Some((database_url, setup_pool, _permit)) = setup_pool().await? else {
        return Ok(());
    };
    let dir = tempfile::tempdir()?;
    let legacy_data_dir = dir.path().join("legacy-data");
    std::fs::create_dir_all(&legacy_data_dir)?;
    let legacy_path = legacy_data_dir.join("events.db");
    let legacy_context =
        PgStoreContext::from_legacy_path_schema(&legacy_path, Some(&database_url))?;

    let legacy_store =
        EventStore::new_with_context(&legacy_data_dir, &legacy_context, &setup_pool).await?;
    let legacy_event = make_test_event("legacy-event", "legacy");
    legacy_store.log(&legacy_event).await?;
    let legacy_watermark = chrono::Utc::now() - chrono::Duration::hours(1);
    legacy_store
        .set_scan_watermark("project", "gc", legacy_watermark)
        .await?;
    legacy_store.close().await;

    let mut target_schema = TestSchemaGuard::new(&database_url, "event_store_backfill_test")?;
    let target_context = PgStoreContext::from_schema(target_schema.schema(), Some(&database_url))?;
    let target_store =
        EventStore::new_shared_with_context(&legacy_data_dir, &target_context, &setup_pool).await?;

    let copied_events = target_store.query(&EventFilters::default()).await?;
    assert_eq!(copied_events.len(), 1);
    assert_eq!(copied_events[0].id, legacy_event.id);
    assert_eq!(
        target_store
            .get_scan_watermark("project", "gc")
            .await?
            .expect("legacy watermark should be copied")
            .timestamp(),
        legacy_watermark.timestamp()
    );

    let stale_legacy_store =
        EventStore::new_with_context(&legacy_data_dir, &legacy_context, &setup_pool).await?;
    stale_legacy_store
        .log(&make_test_event("stale-legacy-event", "stale"))
        .await?;
    stale_legacy_store.close().await;

    let copied_again =
        migrate_legacy_event_store_if_needed(&legacy_path, Some(&database_url), &target_store)
            .await?;
    assert_eq!(copied_again, 0);
    let events_after_second_backfill = target_store.query(&EventFilters::default()).await?;
    assert_eq!(events_after_second_backfill.len(), 1);
    assert_eq!(events_after_second_backfill[0].id, legacy_event.id);

    target_store.close().await;
    target_schema.cleanup_with_pool(&setup_pool).await?;
    setup_pool.close().await;
    Ok(())
}
