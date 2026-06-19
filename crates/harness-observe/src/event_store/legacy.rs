use super::{EventStore, EVENT_MIGRATIONS};
use harness_core::db::PgStoreContext;
use std::path::Path;

pub async fn migrate_legacy_event_store_if_needed(
    legacy_path: &Path,
    configured_database_url: Option<&str>,
    target_store: &EventStore,
) -> anyhow::Result<u64> {
    let legacy_context =
        PgStoreContext::from_legacy_path_schema(legacy_path, configured_database_url)?;
    let legacy_schema = legacy_context.schema();
    if legacy_schema == target_store.schema() {
        return Ok(0);
    }

    let already_backfilled: Option<String> = sqlx::query_scalar(
        "SELECT legacy_schema
         FROM event_store_legacy_backfills
         WHERE store_key = $1 AND legacy_schema = $2",
    )
    .bind(target_store.store_key())
    .bind(legacy_schema)
    .fetch_optional(&target_store.pool)
    .await?;
    if already_backfilled.is_some() {
        return Ok(0);
    }

    let legacy_events_table: Option<String> = sqlx::query_scalar("SELECT to_regclass($1)::text")
        .bind(format!("{}.events", quote_pg_ident(legacy_schema)))
        .fetch_one(&target_store.pool)
        .await?;
    if legacy_events_table.is_none() {
        return Ok(0);
    }

    let legacy_pool = legacy_context.open_migrated_pool(EVENT_MIGRATIONS).await?;
    legacy_pool.close().await;

    let legacy_schema_sql = quote_pg_ident(legacy_schema);
    let mut tx = target_store.pool.begin().await?;
    let events_sql = format!(
        "INSERT INTO events (
            store_key, id, ts, session_id, hook, tool, decision, reason, detail,
            duration_ms, content, metadata
         )
         SELECT $1, id, ts, session_id, hook, tool, decision, reason, detail,
                duration_ms, content, metadata
         FROM {legacy_schema_sql}.events
         ON CONFLICT (store_key, id) DO NOTHING"
    );
    let copied_events = sqlx::query(&events_sql)
        .bind(target_store.store_key())
        .execute(&mut *tx)
        .await?
        .rows_affected();

    let watermarks_sql = format!(
        "INSERT INTO scan_watermarks (store_key, project, agent_id, last_scan_ts)
         SELECT $1, project, agent_id, last_scan_ts
         FROM {legacy_schema_sql}.scan_watermarks
         ON CONFLICT (store_key, project, agent_id) DO NOTHING"
    );
    let copied_watermarks = sqlx::query(&watermarks_sql)
        .bind(target_store.store_key())
        .execute(&mut *tx)
        .await?
        .rows_affected();
    let copied_rows = copied_events + copied_watermarks;

    sqlx::query(
        "INSERT INTO event_store_legacy_backfills (store_key, legacy_schema, copied_rows)
         VALUES ($1, $2, $3)
         ON CONFLICT (store_key, legacy_schema) DO NOTHING",
    )
    .bind(target_store.store_key())
    .bind(legacy_schema)
    .bind(copied_rows as i64)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;

    if copied_rows > 0 {
        tracing::info!(
            copied_events,
            copied_watermarks,
            legacy_schema,
            target_schema = target_store.schema(),
            "event store migration: backfilled legacy rows into shared schema"
        );
    }

    Ok(copied_rows)
}

fn quote_pg_ident(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}
