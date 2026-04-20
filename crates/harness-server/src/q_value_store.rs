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

use harness_core::db::{
    pg_create_schema_if_not_exists, pg_open_pool, pg_open_pool_schematized, Migration, PgMigrator,
};
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
];

/// Persistent store for pipeline events and rule Q-values.
pub struct QValueStore {
    pool: PgPool,
}

impl QValueStore {
    /// Open (or create) the Q-value store at `path`, running any pending migrations.
    pub async fn open(path: &Path) -> anyhow::Result<Self> {
        let database_url = std::env::var("DATABASE_URL")
            .map_err(|_| anyhow::anyhow!("DATABASE_URL environment variable is not set"))?;
        use sha2::{Digest, Sha256};
        let digest = Sha256::digest(path.as_os_str().as_encoded_bytes());
        let mut schema_bytes = [0u8; 8];
        schema_bytes.copy_from_slice(&digest[..8]);
        let schema = format!("h{:016x}", u64::from_le_bytes(schema_bytes));

        let setup = pg_open_pool(&database_url).await?;
        pg_create_schema_if_not_exists(&setup, &schema).await?;
        setup.close().await;

        let pool = pg_open_pool_schematized(&database_url, &schema).await?;
        PgMigrator::new(&pool, Q_VALUE_MIGRATIONS).run().await?;
        Ok(Self { pool })
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
            "INSERT INTO pipeline_events (task_id, phase, experiences_used, created_at)
             VALUES ($1, $2, $3, CURRENT_TIMESTAMP)",
        )
        .bind(task_id)
        .bind(phase)
        .bind(&experiences_json)
        .execute(&mut *tx)
        .await?;

        for rule_id in experience_ids {
            sqlx::query(
                "INSERT INTO rule_experiences (rule_id, retrieval_count, updated_at)
                 VALUES ($1, 1, CURRENT_TIMESTAMP)
                 ON CONFLICT(rule_id) DO UPDATE SET
                   retrieval_count = rule_experiences.retrieval_count + 1,
                   updated_at      = CURRENT_TIMESTAMP",
            )
            .bind(rule_id)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Collect all distinct experience IDs referenced across all pipeline events for a task.
    pub async fn get_experiences_for_task(&self, task_id: &str) -> anyhow::Result<Vec<String>> {
        let rows: Vec<(String,)> =
            sqlx::query_as("SELECT experiences_used FROM pipeline_events WHERE task_id = $1")
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
                "INSERT INTO rule_experiences (rule_id, q_value, success_count, updated_at)
                 VALUES ($1, 0.5 + $2 * ($3 - 0.5), $4, CURRENT_TIMESTAMP)
                 ON CONFLICT(rule_id) DO UPDATE SET
                   q_value       = rule_experiences.q_value + $2 * ($3 - rule_experiences.q_value),
                   success_count = rule_experiences.success_count + $4,
                   updated_at    = CURRENT_TIMESTAMP",
            )
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
        let row: Option<(f64,)> =
            sqlx::query_as("SELECT q_value FROM rule_experiences WHERE rule_id = $1")
                .bind(rule_id)
                .fetch_optional(&self.pool)
                .await?;
        Ok(row.map(|(v,)| v))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    async fn open_test_store() -> anyhow::Result<Option<(QValueStore, tempfile::TempDir)>> {
        if std::env::var("DATABASE_URL").is_err() {
            return Ok(None);
        }
        let dir = tempdir()?;
        let path = dir.path().join("q_values.db");
        let store = QValueStore::open(&path).await?;
        Ok(Some((store, dir)))
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
        // Q_new = 0.5 + 0.1 * (1.0 - 0.5) = 0.55
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
        // Q_new = 0.5 + 0.1 * (0.0 - 0.5) = 0.45
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

        // Apply closed reward — success_count should stay 0.
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

        // Apply merged reward — success_count should become 1.
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
}
