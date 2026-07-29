use chrono::{DateTime, Utc};
use harness_core::config::misc::OtelConfig;
use harness_core::db::{pg_open_pool, PgStoreContext};
use harness_core::types::Event;
#[cfg(test)]
use harness_core::types::{
    AutoFixReport, Decision, EventFilters, ExternalSignal, SessionId, Severity, Violation,
};
use sqlx::postgres::PgPool;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

mod events;
mod external_signals;
#[cfg(test)]
mod hardening_tests;
mod legacy;
mod migrations;
mod policy_logging;
mod trajectory;

pub use legacy::migrate_legacy_event_store_if_needed;
use migrations::EVENT_MIGRATIONS;
pub use migrations::EVENT_STORE_SCHEMA;

/// Event store backed by Postgres.
///
/// Backward compatibility: on first startup the store imports any existing
/// `events.jsonl` file found in the data directory, then renames it as an
/// archive after a successful import.
pub struct EventStore {
    pool: PgPool,
    schema: String,
    store_key: String,
    data_dir: PathBuf,
    otel_pipeline: Mutex<Option<crate::otel_export::OtelPipeline>>,
    session_renewal_secs: u64,
}

impl EventStore {
    pub async fn new(data_dir: &Path) -> anyhow::Result<Self> {
        Self::new_with_database_url(data_dir, None).await
    }

    pub async fn new_with_database_url(
        data_dir: &Path,
        configured_database_url: Option<&str>,
    ) -> anyhow::Result<Self> {
        let context = Self::shared_schema_context(configured_database_url)?;
        let setup_pool = pg_open_pool(context.database_url()).await?;
        let store = Self::new_shared_with_context(data_dir, &context, &setup_pool).await;
        setup_pool.close().await;
        store
    }

    pub fn shared_schema_context(
        configured_database_url: Option<&str>,
    ) -> anyhow::Result<PgStoreContext> {
        PgStoreContext::from_schema(EVENT_STORE_SCHEMA, configured_database_url)
    }

    pub async fn new_with_context(
        data_dir: &Path,
        context: &PgStoreContext,
        setup_pool: &PgPool,
    ) -> anyhow::Result<Self> {
        let store_key = context.schema().to_owned();
        let store =
            Self::new_with_context_and_store_key(data_dir, context, setup_pool, store_key).await?;
        store.migrate_from_jsonl().await;
        Ok(store)
    }

    pub async fn new_shared_with_context(
        data_dir: &Path,
        context: &PgStoreContext,
        setup_pool: &PgPool,
    ) -> anyhow::Result<Self> {
        std::fs::create_dir_all(data_dir)?;
        let store_key = Self::store_key_for_data_dir(data_dir)?;
        let store =
            Self::new_with_context_and_store_key(data_dir, context, setup_pool, store_key).await?;
        migrate_legacy_event_store_if_needed(
            &data_dir.join("events.db"),
            Some(context.database_url()),
            &store,
        )
        .await?;
        store.migrate_from_jsonl().await;
        Ok(store)
    }

    async fn new_with_context_and_store_key(
        data_dir: &Path,
        context: &PgStoreContext,
        setup_pool: &PgPool,
        store_key: String,
    ) -> anyhow::Result<Self> {
        std::fs::create_dir_all(data_dir)?;
        let pool = context
            .open_migrated_pool_with_setup_pool(setup_pool, EVENT_MIGRATIONS)
            .await?;
        Ok(Self {
            pool,
            schema: context.schema().to_owned(),
            store_key,
            data_dir: data_dir.to_path_buf(),
            otel_pipeline: Mutex::new(None),
            session_renewal_secs: 1800,
        })
    }

    pub fn store_key_for_data_dir(data_dir: &Path) -> anyhow::Result<String> {
        let canonical = data_dir.canonicalize().map_err(|error| {
            anyhow::anyhow!(
                "failed to canonicalize event store data_dir {}: {error}",
                data_dir.display()
            )
        })?;
        Ok(canonical.to_string_lossy().into_owned())
    }

    pub fn schema(&self) -> &str {
        &self.schema
    }

    pub fn store_key(&self) -> &str {
        &self.store_key
    }

    /// Close the connection pool.
    pub async fn close(self) {
        self.pool.close().await;
    }

    /// Create a non-functional store for unit tests that need an `&EventStore`
    /// but do not care about event persistence. All `log` calls will fail and
    /// callers that handle those errors (e.g. with `tracing::warn!`) will
    /// continue normally. Do not use outside of test code.
    #[doc(hidden)]
    pub fn new_noop_for_tests() -> Self {
        let pool = match PgPool::connect_lazy("postgresql://localhost/harness_noop") {
            Ok(pool) => pool,
            Err(error) => panic!("lazy pool URL must be syntactically valid: {error}"),
        };
        Self {
            pool,
            schema: String::new(),
            store_key: String::new(),
            data_dir: PathBuf::new(),
            otel_pipeline: Mutex::new(None),
            session_renewal_secs: 1800,
        }
    }

    pub fn session_renewal_secs(&self) -> u64 {
        self.session_renewal_secs
    }

    /// Delete all events whose timestamp is older than `days` days.
    ///
    /// Phase 1: deletes regular events older than the retention window, sparing
    /// periodic_review:* and periodic_retry:* hooks (used as watermark cursors).
    /// Phase 2: keeps only the newest row per watermark hook.
    ///
    /// Returns the number of rows deleted.  A `days` value of 0 is a no-op.
    pub async fn purge_old_events(&self, days: u32) -> anyhow::Result<u64> {
        if days == 0 {
            return Ok(0);
        }
        let cutoff = chrono::Utc::now() - chrono::Duration::days(i64::from(days));
        let mut total_deleted: u64 = 0;

        loop {
            let result = sqlx::query(
                "DELETE FROM events WHERE store_key = $1 AND id IN (
                    SELECT id FROM events
                    WHERE store_key = $1
                      AND ts < $2
                      AND hook NOT LIKE 'periodic_review:%'
                      AND hook NOT LIKE 'periodic_retry:%'
                    LIMIT 500
                )",
            )
            .bind(&self.store_key)
            .bind(cutoff)
            .execute(&self.pool)
            .await?;
            let batch = result.rows_affected();
            total_deleted += batch;
            if batch == 0 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        loop {
            let result = sqlx::query(
                "DELETE FROM events WHERE store_key = $1 AND id IN (
                    SELECT e.id FROM events e
                    WHERE e.store_key = $1
                    AND (e.hook LIKE 'periodic_review:%' OR e.hook = 'periodic_retry:summary')
                    AND e.ts < (
                        SELECT MAX(e2.ts) FROM events e2
                        WHERE e2.store_key = $1 AND e2.hook = e.hook
                    )
                    LIMIT 500
                )",
            )
            .bind(&self.store_key)
            .execute(&self.pool)
            .await?;
            let batch = result.rows_affected();
            total_deleted += batch;
            if batch == 0 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        if total_deleted > 0 {
            tracing::info!(
                deleted = total_deleted,
                days,
                "event store: purged old events"
            );
        }
        Ok(total_deleted)
    }

    pub async fn with_policies_and_otel(
        data_dir: &Path,
        session_renewal_secs: u64,
        log_retention_days: u32,
        otel_config: &OtelConfig,
    ) -> anyhow::Result<Self> {
        Self::with_policies_and_otel_with_database_url(
            data_dir,
            None,
            session_renewal_secs,
            log_retention_days,
            otel_config,
        )
        .await
    }

    pub async fn with_policies_and_otel_with_database_url(
        data_dir: &Path,
        configured_database_url: Option<&str>,
        session_renewal_secs: u64,
        log_retention_days: u32,
        otel_config: &OtelConfig,
    ) -> anyhow::Result<Self> {
        let mut store = Self::new_with_database_url(data_dir, configured_database_url).await?;
        store
            .apply_policies_and_otel(session_renewal_secs, log_retention_days, otel_config)
            .await?;
        Ok(store)
    }

    pub async fn with_policies_and_otel_with_context(
        data_dir: &Path,
        context: &PgStoreContext,
        setup_pool: &PgPool,
        session_renewal_secs: u64,
        log_retention_days: u32,
        otel_config: &OtelConfig,
    ) -> anyhow::Result<Self> {
        let mut store = Self::new_with_context(data_dir, context, setup_pool).await?;
        store
            .apply_policies_and_otel(session_renewal_secs, log_retention_days, otel_config)
            .await?;
        Ok(store)
    }

    pub async fn with_policies_and_otel_shared_with_context(
        data_dir: &Path,
        context: &PgStoreContext,
        setup_pool: &PgPool,
        session_renewal_secs: u64,
        log_retention_days: u32,
        otel_config: &OtelConfig,
    ) -> anyhow::Result<Self> {
        let mut store = Self::new_shared_with_context(data_dir, context, setup_pool).await?;
        store
            .apply_policies_and_otel(session_renewal_secs, log_retention_days, otel_config)
            .await?;
        Ok(store)
    }

    async fn apply_policies_and_otel(
        &mut self,
        session_renewal_secs: u64,
        log_retention_days: u32,
        otel_config: &OtelConfig,
    ) -> anyhow::Result<()> {
        self.session_renewal_secs = session_renewal_secs;
        tracing::debug!(
            session_renewal_secs,
            log_retention_days,
            "event store: applying retention policies"
        );
        if let Err(e) = self.purge_old_events(log_retention_days).await {
            tracing::warn!("event store: failed to purge old events: {e}");
        }
        let pipeline = match crate::otel_export::OtelPipeline::from_config(otel_config).await {
            Ok(pipeline) => pipeline,
            Err(err) => {
                tracing::warn!(
                    "OpenTelemetry initialization failed; continuing without export: {err}"
                );
                None
            }
        };
        *self.otel_pipeline.lock().unwrap_or_else(|e| e.into_inner()) = pipeline;
        Ok(())
    }

    /// Import events from an existing `events.jsonl` file (backward compat).
    ///
    /// The JSONL file is archived only after all parsed events are inserted.
    /// That keeps a partially failed batch migration retryable: already
    /// inserted rows hit `ON CONFLICT DO NOTHING`, and remaining rows can still
    /// be imported on the next startup.
    async fn migrate_from_jsonl(&self) {
        use std::io::BufRead as _;

        let path = self.data_dir.join("events.jsonl");
        let archive_path = self.data_dir.join("events.jsonl.migrated");
        let file = match std::fs::File::open(&path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return;
            }
            Err(e) => {
                tracing::warn!("event store: could not open events.jsonl for migration: {e}");
                return;
            }
        };
        const JSONL_MIGRATION_BATCH_SIZE: usize = 1_000;
        let mut pending = Vec::with_capacity(JSONL_MIGRATION_BATCH_SIZE);
        let mut imported = 0usize;
        for line in std::io::BufReader::new(file).lines() {
            let line = match line {
                Ok(l) => l,
                Err(e) => {
                    tracing::warn!(
                        "event store: I/O error reading events.jsonl, aborting migration: {e}"
                    );
                    break;
                }
            };
            let line = line.trim().to_owned();
            if line.is_empty() {
                continue;
            }
            if let Ok(event) = serde_json::from_str::<Event>(&line) {
                pending.push(event);
                if pending.len() >= JSONL_MIGRATION_BATCH_SIZE {
                    if let Err(e) = self.insert_events(&pending).await {
                        tracing::warn!("event store: failed to batch insert migrated events: {e}");
                        return;
                    }
                    imported += pending.len();
                    pending.clear();
                }
            }
        }
        if !pending.is_empty() {
            if let Err(e) = self.insert_events(&pending).await {
                tracing::warn!("event store: failed to batch insert migrated events: {e}");
                return;
            }
            imported += pending.len();
        }
        if let Err(e) = std::fs::rename(&path, &archive_path) {
            tracing::warn!(
                "event store: migrated events.jsonl but could not archive {} to {}: {e}",
                path.display(),
                archive_path.display()
            );
            return;
        }
        if imported > 0 {
            tracing::info!(
                imported,
                archive = %archive_path.display(),
                "event store: migrated events from events.jsonl"
            );
        }
    }

    pub async fn shutdown(&self) {
        let pipeline = self
            .otel_pipeline
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        if let Some(pipeline) = pipeline {
            pipeline.shutdown().await;
        }
    }

    pub async fn get_scan_watermark(
        &self,
        project: &str,
        agent_id: &str,
    ) -> anyhow::Result<Option<DateTime<Utc>>> {
        let row: Option<(String,)> = sqlx::query_as(
            "SELECT last_scan_ts FROM scan_watermarks
             WHERE store_key = $1 AND project = $2 AND agent_id = $3",
        )
        .bind(&self.store_key)
        .bind(project)
        .bind(agent_id)
        .fetch_optional(&self.pool)
        .await?;
        match row {
            None => Ok(None),
            Some((ts_str,)) => {
                let ts = chrono::DateTime::parse_from_rfc3339(&ts_str)
                    .map_err(|e| anyhow::anyhow!("invalid watermark ts '{ts_str}': {e}"))?
                    .with_timezone(&chrono::Utc);
                Ok(Some(ts))
            }
        }
    }

    pub async fn set_scan_watermark(
        &self,
        project: &str,
        agent_id: &str,
        ts: DateTime<Utc>,
    ) -> anyhow::Result<()> {
        sqlx::query(
            "INSERT INTO scan_watermarks (store_key, project, agent_id, last_scan_ts)
             VALUES ($1, $2, $3, $4)
             ON CONFLICT (store_key, project, agent_id)
             DO UPDATE SET last_scan_ts = EXCLUDED.last_scan_ts",
        )
        .bind(&self.store_key)
        .bind(project)
        .bind(agent_id)
        .bind(ts.to_rfc3339())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn otel_pipeline_is_none(&self) -> bool {
        self.otel_pipeline
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_none()
    }
}

#[cfg(test)]
mod shared_schema_tests;

#[cfg(test)]
mod store_tests;
