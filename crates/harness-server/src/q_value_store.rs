//! Q-value utility tracking for rule/experience memory (MemRL pattern).
//!
//! Implements the memory-based reinforcement learning approach from MemRL
//! (arxiv:2601.03192): instead of updating model parameters, only evolve the
//! memory layer Q-values. Rules with high utility naturally rise; unused or
//! harmful rules fall — replacing blind confidence decay.
//!
//! ## Schema
//!
//! - **`pipeline_events`**: records which rule/experience IDs were used at each
//!   pipeline phase for a given task.
//! - **`rule_experiences`**: per-rule Q-value tracking with retrieval and success counts.
//!
//! ## Update formula
//!
//! ```text
//! Q_new = Q_old + alpha * (reward - Q_old)
//! ```
//!
//! Reward values:
//! - `merged`        → 1.0 (PR accepted)
//! - `closed`        → 0.0 (PR rejected/abandoned)
//! - `unknown_closed`→ 0.2 (terminal state but outcome unclear)

use harness_core::db::{Migration, PgStoreContext};
use harness_core::store_backend::{PostgresBackend, StoreLocation};
use sqlx::postgres::PgPool;
use std::path::Path;

/// Default learning rate for Q-value updates.
pub const DEFAULT_ALPHA: f64 = 0.1;

/// Reward for a merged PR (rule was useful).
pub const REWARD_MERGED: f64 = 1.0;

/// Reward for a closed-without-merge PR (rule was not useful).
pub const REWARD_CLOSED: f64 = 0.0;

/// Reward when PR terminal state is unknown (e.g. server outage during close).
pub const REWARD_UNKNOWN_CLOSED: f64 = 0.2;

pub const Q_VALUE_STORE_SCHEMA: &str = "q_value_store";

static Q_VALUE_MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        description: "create pipeline_events table",
        sql: "CREATE TABLE IF NOT EXISTS pipeline_events (
            id               BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
            task_id          TEXT NOT NULL,
            phase            TEXT NOT NULL,
            experiences_used TEXT NOT NULL DEFAULT '[]',
            created_at       TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP
        )",
    },
    Migration {
        version: 2,
        description: "create rule_experiences table for Q-value tracking",
        sql: "CREATE TABLE IF NOT EXISTS rule_experiences (
            rule_id         TEXT PRIMARY KEY,
            q_value         DOUBLE PRECISION NOT NULL DEFAULT 0.5,
            retrieval_count BIGINT NOT NULL DEFAULT 0,
            success_count   BIGINT NOT NULL DEFAULT 0,
            updated_at      TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP
        )",
    },
    Migration {
        version: 3,
        description: "add index on pipeline_events(task_id) for task lookup",
        sql: "CREATE INDEX IF NOT EXISTS idx_pipeline_events_task_id \
              ON pipeline_events(task_id)",
    },
    Migration {
        version: 4,
        description: "scope q-value rows within shared schema",
        sql: "ALTER TABLE pipeline_events ADD COLUMN IF NOT EXISTS store_key TEXT;
              UPDATE pipeline_events
              SET store_key = COALESCE(NULLIF(store_key, ''), current_schema())
              WHERE store_key IS NULL OR store_key = '';
              ALTER TABLE pipeline_events ALTER COLUMN store_key SET DEFAULT current_schema();
              ALTER TABLE pipeline_events ALTER COLUMN store_key SET NOT NULL;
              DROP INDEX IF EXISTS idx_pipeline_events_task_id;
              CREATE INDEX IF NOT EXISTS idx_pipeline_events_store_task_id
              ON pipeline_events(store_key, task_id);

              ALTER TABLE rule_experiences ADD COLUMN IF NOT EXISTS store_key TEXT;
              UPDATE rule_experiences
              SET store_key = COALESCE(NULLIF(store_key, ''), current_schema())
              WHERE store_key IS NULL OR store_key = '';
              ALTER TABLE rule_experiences ALTER COLUMN store_key SET DEFAULT current_schema();
              ALTER TABLE rule_experiences ALTER COLUMN store_key SET NOT NULL;
              ALTER TABLE rule_experiences DROP CONSTRAINT IF EXISTS rule_experiences_pkey;
              ALTER TABLE rule_experiences ADD CONSTRAINT rule_experiences_pkey
              PRIMARY KEY (store_key, rule_id)",
    },
    Migration {
        version: 5,
        description: "record legacy q-value store backfills",
        sql: "CREATE TABLE IF NOT EXISTS q_value_store_legacy_backfills (
            store_key     TEXT NOT NULL DEFAULT current_schema(),
            legacy_schema TEXT NOT NULL,
            copied_rows   BIGINT NOT NULL DEFAULT 0,
            backfilled_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
            PRIMARY KEY (store_key, legacy_schema)
        )",
    },
];

/// Persistent store for pipeline events and rule Q-values.
pub struct QValueStore {
    pool: PgPool,
    schema: String,
    store_key: String,
}

impl QValueStore {
    /// Open (or create) the Q-value store at `path`, running any pending migrations.
    pub async fn open(path: &Path) -> anyhow::Result<Self> {
        Self::open_with_database_url(path, None).await
    }

    pub async fn open_with_database_url(
        path: &Path,
        configured_database_url: Option<&str>,
    ) -> anyhow::Result<Self> {
        let context = PgStoreContext::from_path(path, configured_database_url)?;
        let schema = context.schema().to_owned();
        let store_key = schema.clone();
        let pool = context.open_migrated_pool(Q_VALUE_MIGRATIONS).await?;
        Ok(Self {
            pool,
            schema,
            store_key,
        })
    }

    pub fn shared_schema_context(
        configured_database_url: Option<&str>,
    ) -> anyhow::Result<PgStoreContext> {
        PostgresBackend::new(configured_database_url.map(ToOwned::to_owned)).store_context(
            &StoreLocation::SharedSchema(Q_VALUE_STORE_SCHEMA.to_string()),
        )
    }

    pub async fn open_with_context(
        context: &PgStoreContext,
        setup_pool: &PgPool,
    ) -> anyhow::Result<Self> {
        Self::open_with_context_and_store_key(context, setup_pool, context.schema().to_owned())
            .await
    }

    pub async fn open_shared_with_data_dir(
        context: &PgStoreContext,
        setup_pool: &PgPool,
        data_dir: &Path,
    ) -> anyhow::Result<Self> {
        let store_key = Self::store_key_for_data_dir(data_dir)?;
        Self::open_with_context_and_store_key(context, setup_pool, store_key).await
    }

    async fn open_with_context_and_store_key(
        context: &PgStoreContext,
        setup_pool: &PgPool,
        store_key: String,
    ) -> anyhow::Result<Self> {
        let schema = context.schema().to_owned();
        let pool = context
            .open_migrated_pool_with_setup_pool(setup_pool, Q_VALUE_MIGRATIONS)
            .await?;
        Ok(Self {
            pool,
            schema,
            store_key,
        })
    }

    pub fn schema(&self) -> &str {
        &self.schema
    }

    pub fn store_key_for_data_dir(data_dir: &Path) -> anyhow::Result<String> {
        let canonical = data_dir.canonicalize().map_err(|error| {
            anyhow::anyhow!(
                "failed to canonicalize q_value_store data_dir {}: {error}",
                data_dir.display()
            )
        })?;
        Ok(canonical.to_string_lossy().into_owned())
    }

    pub fn store_key(&self) -> &str {
        &self.store_key
    }

    /// Record which rule/experience IDs were used during a pipeline phase.
    ///
    /// Also increments `retrieval_count` for each referenced rule so that
    /// total retrieval statistics are kept up-to-date.
    pub async fn record_pipeline_event(
        &self,
        task_id: &str,
        phase: &str,
        experience_ids: &[&str],
    ) -> anyhow::Result<()> {
        let experiences_json = serde_json::to_string(experience_ids)?;
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "INSERT INTO pipeline_events (store_key, task_id, phase, experiences_used, created_at)
             VALUES ($1, $2, $3, $4, CURRENT_TIMESTAMP)",
        )
        .bind(&self.store_key)
        .bind(task_id)
        .bind(phase)
        .bind(&experiences_json)
        .execute(&mut *tx)
        .await?;

        for rule_id in experience_ids {
            sqlx::query(
                "INSERT INTO rule_experiences (store_key, rule_id, retrieval_count, updated_at)
                 VALUES ($1, $2, 1, CURRENT_TIMESTAMP)
                 ON CONFLICT(store_key, rule_id) DO UPDATE SET
                   retrieval_count = rule_experiences.retrieval_count + 1,
                   updated_at      = CURRENT_TIMESTAMP",
            )
            .bind(&self.store_key)
            .bind(rule_id)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Collect all distinct experience IDs referenced across all pipeline events for a task.
    pub async fn get_experiences_for_task(&self, task_id: &str) -> anyhow::Result<Vec<String>> {
        let rows: Vec<(String,)> = sqlx::query_as(
            "SELECT experiences_used
             FROM pipeline_events
             WHERE store_key = $1 AND task_id = $2",
        )
        .bind(&self.store_key)
        .bind(task_id)
        .fetch_all(&self.pool)
        .await?;

        let mut ids = Vec::new();
        for (json,) in rows {
            let parsed: Vec<String> = serde_json::from_str(&json).unwrap_or_else(|e| {
                let truncated = if json.len() > 200 {
                    let end = (0..=200)
                        .rev()
                        .find(|&i| json.is_char_boundary(i))
                        .unwrap_or(0);
                    format!("{}…({} bytes total)", &json[..end], json.len())
                } else {
                    json.clone()
                };
                tracing::error!(
                    task_id = %task_id,
                    json = %truncated,
                    error = %e,
                    "Failed to parse experiences_used JSON; skipping row"
                );
                Vec::new()
            });
            ids.extend(parsed);
        }
        ids.sort();
        ids.dedup();
        Ok(ids)
    }

    /// Apply Q-value updates using the MemRL formula:
    /// `Q_new = Q_old + alpha * (reward - Q_old)`
    ///
    /// Also increments `success_count` when `reward >= 1.0` (merged PR).
    /// No-op when `experience_ids` is empty.
    pub async fn apply_q_update(
        &self,
        experience_ids: &[String],
        reward: f64,
        alpha: f64,
    ) -> anyhow::Result<()> {
        if experience_ids.is_empty() {
            return Ok(());
        }
        let success_delta: i64 = if reward >= 1.0 { 1 } else { 0 };
        let mut tx = self.pool.begin().await?;
        for rule_id in experience_ids {
            sqlx::query(
                "INSERT INTO rule_experiences (store_key, rule_id, q_value, success_count, updated_at)
                 VALUES ($1, $2, 0.5 + $3 * ($4 - 0.5), $5, CURRENT_TIMESTAMP)
                 ON CONFLICT(store_key, rule_id) DO UPDATE SET
                   q_value       = rule_experiences.q_value + $3 * ($4 - rule_experiences.q_value),
                   success_count = rule_experiences.success_count + $5,
                   updated_at    = CURRENT_TIMESTAMP",
            )
            .bind(&self.store_key)
            .bind(rule_id)
            .bind(alpha)
            .bind(reward)
            .bind(success_delta)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        tracing::debug!(
            experience_count = experience_ids.len(),
            reward,
            alpha,
            "q_value_store: applied Q-value update"
        );
        Ok(())
    }

    /// Return the current Q-value for a rule, or `None` if no record exists.
    pub async fn q_value_for(&self, rule_id: &str) -> anyhow::Result<Option<f64>> {
        let row: Option<(f64,)> = sqlx::query_as(
            "SELECT q_value
             FROM rule_experiences
             WHERE store_key = $1 AND rule_id = $2",
        )
        .bind(&self.store_key)
        .bind(rule_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|(v,)| v))
    }

    #[cfg(test)]
    pub(crate) fn schema_for_test(&self) -> &str {
        &self.schema
    }

    #[cfg(test)]
    pub(crate) fn store_key_for_test(&self) -> &str {
        &self.store_key
    }
}

pub async fn migrate_legacy_q_value_store_if_needed(
    legacy_path: &Path,
    configured_database_url: Option<&str>,
    target_store: &QValueStore,
) -> anyhow::Result<u64> {
    let legacy_context = PgStoreContext::from_path(legacy_path, configured_database_url)?;
    let legacy_schema = legacy_context.schema();
    if legacy_schema == target_store.schema() {
        return Ok(0);
    }

    let legacy_schema_exists: bool = sqlx::query_scalar(
        "SELECT EXISTS (
             SELECT 1
             FROM pg_catalog.pg_namespace
             WHERE nspname = $1
         )",
    )
    .bind(legacy_schema)
    .fetch_one(&target_store.pool)
    .await?;
    if !legacy_schema_exists {
        return Ok(0);
    }

    let already_backfilled: Option<String> = sqlx::query_scalar(
        "SELECT legacy_schema
         FROM q_value_store_legacy_backfills
         WHERE store_key = $1 AND legacy_schema = $2",
    )
    .bind(target_store.store_key())
    .bind(legacy_schema)
    .fetch_optional(&target_store.pool)
    .await?;
    if already_backfilled.is_some() {
        return Ok(0);
    }

    let legacy_pipeline_events: Option<String> = sqlx::query_scalar("SELECT to_regclass($1)::text")
        .bind(format!("{}.pipeline_events", quote_pg_ident(legacy_schema)))
        .fetch_one(&target_store.pool)
        .await?;
    let legacy_rule_experiences: Option<String> =
        sqlx::query_scalar("SELECT to_regclass($1)::text")
            .bind(format!(
                "{}.rule_experiences",
                quote_pg_ident(legacy_schema)
            ))
            .fetch_one(&target_store.pool)
            .await?;
    if legacy_pipeline_events.is_none() && legacy_rule_experiences.is_none() {
        return Ok(0);
    }

    let legacy_schema_sql = quote_pg_ident(legacy_schema);
    let mut copied = 0;
    let mut tx = target_store.pool.begin().await?;

    if legacy_pipeline_events.is_some() {
        let copy_pipeline_events_sql = format!(
            "INSERT INTO pipeline_events (store_key, task_id, phase, experiences_used, created_at)
             SELECT $1, task_id, phase, experiences_used, created_at
             FROM {legacy_schema_sql}.pipeline_events
             ORDER BY id"
        );
        copied += sqlx::query(&copy_pipeline_events_sql)
            .bind(target_store.store_key())
            .execute(&mut *tx)
            .await?
            .rows_affected();
    }

    if legacy_rule_experiences.is_some() {
        let copy_rule_experiences_sql = format!(
            "INSERT INTO rule_experiences (
                store_key, rule_id, q_value, retrieval_count, success_count, updated_at
             )
             SELECT $1, rule_id, q_value, retrieval_count, success_count, updated_at
             FROM {legacy_schema_sql}.rule_experiences
             ON CONFLICT (store_key, rule_id) DO NOTHING"
        );
        copied += sqlx::query(&copy_rule_experiences_sql)
            .bind(target_store.store_key())
            .execute(&mut *tx)
            .await?
            .rows_affected();
    }

    sqlx::query(
        "INSERT INTO q_value_store_legacy_backfills (store_key, legacy_schema, copied_rows)
         VALUES ($1, $2, $3)
         ON CONFLICT (store_key, legacy_schema) DO NOTHING",
    )
    .bind(target_store.store_key())
    .bind(legacy_schema)
    .bind(copied as i64)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;

    if copied > 0 {
        tracing::info!(
            copied,
            legacy_schema,
            target_schema = target_store.schema(),
            "q_value store migration: backfilled legacy rows into shared schema"
        );
    }

    Ok(copied)
}

fn quote_pg_ident(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

#[cfg(test)]
#[path = "q_value_store_tests.rs"]
mod tests;
