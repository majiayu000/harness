use super::super::model::RuntimeKind;
use super::WorkflowRuntimeStore;
use chrono::{DateTime, Utc};
use harness_observe::usage::UsageMetrics;

pub type RuntimeUsageMetrics = UsageMetrics;
const COST_USD_MICROS_PER_DOLLAR: f64 = 1_000_000.0;

pub fn cost_usd_to_micros(cost_usd: f64) -> anyhow::Result<u64> {
    if !cost_usd.is_finite() || cost_usd < 0.0 {
        anyhow::bail!("cost_usd must be finite and nonnegative");
    }
    let cost_usd_micros = (cost_usd * COST_USD_MICROS_PER_DOLLAR).round();
    if cost_usd_micros >= i64::MAX as f64 {
        anyhow::bail!("cost_usd exceeds the runtime usage storage range");
    }
    Ok(cost_usd_micros as u64)
}

pub fn cost_usd_from_micros(cost_usd_micros: u64) -> f64 {
    cost_usd_micros as f64 / COST_USD_MICROS_PER_DOLLAR
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeUsageUpsert {
    pub runtime_job_id: String,
    pub command_id: String,
    pub workflow_id: String,
    pub turn_id: Option<String>,
    pub runtime_kind: RuntimeKind,
    pub runtime_profile: String,
    pub agent: String,
    pub model: String,
    pub project: String,
    pub task_id: Option<String>,
    pub candidate_group_id: Option<String>,
    pub candidate_id: Option<String>,
    pub candidate_index: Option<u32>,
    pub candidate_count: Option<u32>,
    pub metrics: RuntimeUsageMetrics,
    pub cost_usd_micros: u64,
    pub reported_at: DateTime<Utc>,
}

impl RuntimeUsageUpsert {
    fn usage_key(&self) -> String {
        self.turn_id
            .as_deref()
            .filter(|turn_id| !turn_id.trim().is_empty())
            .map(|turn_id| format!("turn:{turn_id}"))
            .unwrap_or_else(|| "runtime_job".to_string())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeUsageUpsertOutcome {
    SkippedZeroUsage,
    Persisted,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeUsageRecord {
    pub id: String,
    pub runtime_job_id: String,
    pub usage_key: String,
    pub command_id: String,
    pub workflow_id: String,
    pub turn_id: Option<String>,
    pub runtime_kind: String,
    pub runtime_profile: String,
    pub agent: String,
    pub model: String,
    pub project: String,
    pub task_id: Option<String>,
    pub candidate_group_id: Option<String>,
    pub candidate_id: Option<String>,
    pub candidate_index: Option<u32>,
    pub candidate_count: Option<u32>,
    pub metrics: RuntimeUsageMetrics,
    pub cost_usd_micros: u64,
    pub reported_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeWorkflowUsage {
    pub metrics: RuntimeUsageMetrics,
    pub cost_usd_micros: u64,
}

#[derive(sqlx::FromRow)]
struct RuntimeUsageDbRow {
    id: String,
    runtime_job_id: String,
    usage_key: String,
    command_id: String,
    workflow_id: String,
    turn_id: Option<String>,
    runtime_kind: String,
    runtime_profile: String,
    agent: String,
    model: String,
    project: String,
    task_id: Option<String>,
    candidate_group_id: Option<String>,
    candidate_id: Option<String>,
    candidate_index: Option<i64>,
    candidate_count: Option<i64>,
    input_tokens: i64,
    output_tokens: i64,
    cache_read_input_tokens: i64,
    cache_creation_input_tokens: i64,
    reported_total_tokens: Option<i64>,
    cost_usd_micros: i64,
    reported_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

impl WorkflowRuntimeStore {
    pub async fn upsert_runtime_usage(
        &self,
        usage: &RuntimeUsageUpsert,
    ) -> anyhow::Result<RuntimeUsageUpsertOutcome> {
        if usage_metrics_are_zero(&usage.metrics) && usage.cost_usd_micros == 0 {
            return Ok(RuntimeUsageUpsertOutcome::SkippedZeroUsage);
        }
        let usage_key = usage.usage_key();
        let id = format!("runtime_usage:{}:{usage_key}", usage.runtime_job_id);
        sqlx::query(
            "INSERT INTO runtime_usage_events
                (id, runtime_job_id, usage_key, command_id, workflow_id, turn_id,
                 runtime_kind, runtime_profile, agent, model, project, task_id,
                 candidate_group_id, candidate_id, candidate_index, candidate_count,
                 input_tokens, output_tokens, cache_read_input_tokens,
                 cache_creation_input_tokens, reported_total_tokens, cost_usd_micros, reported_at)
             VALUES
                ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12,
                 $13, $14, $15, $16, $17, $18, $19, $20, $21, $22, $23)
             ON CONFLICT (runtime_job_id, usage_key) DO UPDATE SET
                command_id = EXCLUDED.command_id,
                workflow_id = EXCLUDED.workflow_id,
                turn_id = EXCLUDED.turn_id,
                runtime_kind = EXCLUDED.runtime_kind,
                runtime_profile = EXCLUDED.runtime_profile,
                agent = EXCLUDED.agent,
                model = EXCLUDED.model,
                project = EXCLUDED.project,
                task_id = EXCLUDED.task_id,
                candidate_group_id = EXCLUDED.candidate_group_id,
                candidate_id = EXCLUDED.candidate_id,
                candidate_index = EXCLUDED.candidate_index,
                candidate_count = EXCLUDED.candidate_count,
                input_tokens = EXCLUDED.input_tokens,
                output_tokens = EXCLUDED.output_tokens,
                cache_read_input_tokens = EXCLUDED.cache_read_input_tokens,
                cache_creation_input_tokens = EXCLUDED.cache_creation_input_tokens,
                reported_total_tokens = EXCLUDED.reported_total_tokens,
                cost_usd_micros = EXCLUDED.cost_usd_micros,
                reported_at = EXCLUDED.reported_at,
                updated_at = CURRENT_TIMESTAMP",
        )
        .bind(&id)
        .bind(&usage.runtime_job_id)
        .bind(&usage_key)
        .bind(&usage.command_id)
        .bind(&usage.workflow_id)
        .bind(&usage.turn_id)
        .bind(usage.runtime_kind.as_str())
        .bind(&usage.runtime_profile)
        .bind(&usage.agent)
        .bind(&usage.model)
        .bind(&usage.project)
        .bind(&usage.task_id)
        .bind(&usage.candidate_group_id)
        .bind(&usage.candidate_id)
        .bind(usage.candidate_index.map(i64::from))
        .bind(usage.candidate_count.map(i64::from))
        .bind(u64_to_i64(usage.metrics.input_tokens, "input_tokens")?)
        .bind(u64_to_i64(usage.metrics.output_tokens, "output_tokens")?)
        .bind(u64_to_i64(
            usage.metrics.cache_read_input_tokens,
            "cache_read_input_tokens",
        )?)
        .bind(u64_to_i64(
            usage.metrics.cache_creation_input_tokens,
            "cache_creation_input_tokens",
        )?)
        .bind(
            usage
                .metrics
                .reported_total_tokens
                .map(|value| u64_to_i64(value, "reported_total_tokens"))
                .transpose()?,
        )
        .bind(u64_to_i64(usage.cost_usd_micros, "cost_usd_micros")?)
        .bind(usage.reported_at)
        .execute(&self.pool)
        .await?;
        Ok(RuntimeUsageUpsertOutcome::Persisted)
    }

    pub async fn runtime_usage_between(
        &self,
        since: DateTime<Utc>,
        until: DateTime<Utc>,
    ) -> anyhow::Result<Vec<RuntimeUsageRecord>> {
        let rows: Vec<RuntimeUsageDbRow> = sqlx::query_as(
            "SELECT
                id, runtime_job_id, usage_key, command_id, workflow_id, turn_id,
                runtime_kind, runtime_profile, agent, model, project, task_id,
                candidate_group_id, candidate_id, candidate_index, candidate_count,
                input_tokens, output_tokens, cache_read_input_tokens,
                cache_creation_input_tokens, reported_total_tokens, cost_usd_micros,
                reported_at, updated_at
             FROM runtime_usage_events
             WHERE reported_at >= $1 AND reported_at <= $2
             ORDER BY reported_at DESC, id DESC",
        )
        .bind(since)
        .bind(until)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(runtime_usage_record_from_row)
            .collect()
    }

    pub async fn runtime_usage_for_workflow(
        &self,
        workflow_id: &str,
    ) -> anyhow::Result<Option<RuntimeWorkflowUsage>> {
        let row: (i64, i64, i64, i64, i64, i64, i64) = sqlx::query_as(
            "SELECT
                COUNT(*)::BIGINT,
                COALESCE(SUM(input_tokens), 0)::BIGINT,
                COALESCE(SUM(output_tokens), 0)::BIGINT,
                COALESCE(SUM(cache_read_input_tokens), 0)::BIGINT,
                COALESCE(SUM(cache_creation_input_tokens), 0)::BIGINT,
                COALESCE(SUM(GREATEST(
                    COALESCE(reported_total_tokens, 0),
                    input_tokens + output_tokens
                        + cache_read_input_tokens + cache_creation_input_tokens
                )), 0)::BIGINT,
                COALESCE(SUM(cost_usd_micros), 0)::BIGINT
             FROM runtime_usage_events
             WHERE workflow_id = $1",
        )
        .bind(workflow_id)
        .fetch_one(&self.pool)
        .await?;
        if row.0 == 0 {
            return Ok(None);
        }
        Ok(Some(RuntimeWorkflowUsage {
            metrics: RuntimeUsageMetrics {
                input_tokens: i64_to_u64(row.1, "input_tokens")?,
                output_tokens: i64_to_u64(row.2, "output_tokens")?,
                cache_read_input_tokens: i64_to_u64(row.3, "cache_read_input_tokens")?,
                cache_creation_input_tokens: i64_to_u64(row.4, "cache_creation_input_tokens")?,
                reported_total_tokens: Some(i64_to_u64(row.5, "reported_total_tokens")?),
            },
            cost_usd_micros: i64_to_u64(row.6, "cost_usd_micros")?,
        }))
    }

    /// Return one durable runtime turn count per workflow for dashboard
    /// distribution metrics. Each persisted usage key represents one turn (or
    /// the runtime job fallback when an adapter does not expose a turn id).
    pub async fn runtime_turn_counts(&self) -> anyhow::Result<Vec<u32>> {
        let rows: Vec<(i64,)> = sqlx::query_as(
            "SELECT COUNT(*)
             FROM runtime_usage_events
             GROUP BY workflow_id
             ORDER BY COUNT(*) ASC, workflow_id ASC",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|(count,)| {
                u32::try_from(count)
                    .map_err(|_| anyhow::anyhow!("runtime turn count is outside u32 range"))
            })
            .collect()
    }
}

fn runtime_usage_record_from_row(row: RuntimeUsageDbRow) -> anyhow::Result<RuntimeUsageRecord> {
    Ok(RuntimeUsageRecord {
        id: row.id,
        runtime_job_id: row.runtime_job_id,
        usage_key: row.usage_key,
        command_id: row.command_id,
        workflow_id: row.workflow_id,
        turn_id: row.turn_id,
        runtime_kind: row.runtime_kind,
        runtime_profile: row.runtime_profile,
        agent: row.agent,
        model: row.model,
        project: row.project,
        task_id: row.task_id,
        candidate_group_id: row.candidate_group_id,
        candidate_id: row.candidate_id,
        candidate_index: row
            .candidate_index
            .map(|value| i64_to_u32(value, "candidate_index"))
            .transpose()?,
        candidate_count: row
            .candidate_count
            .map(|value| i64_to_u32(value, "candidate_count"))
            .transpose()?,
        metrics: RuntimeUsageMetrics {
            input_tokens: i64_to_u64(row.input_tokens, "input_tokens")?,
            output_tokens: i64_to_u64(row.output_tokens, "output_tokens")?,
            cache_read_input_tokens: i64_to_u64(
                row.cache_read_input_tokens,
                "cache_read_input_tokens",
            )?,
            cache_creation_input_tokens: i64_to_u64(
                row.cache_creation_input_tokens,
                "cache_creation_input_tokens",
            )?,
            reported_total_tokens: row
                .reported_total_tokens
                .map(|value| i64_to_u64(value, "reported_total_tokens"))
                .transpose()?,
        },
        cost_usd_micros: i64_to_u64(row.cost_usd_micros, "cost_usd_micros")?,
        reported_at: row.reported_at,
        updated_at: row.updated_at,
    })
}

fn usage_metrics_are_zero(metrics: &RuntimeUsageMetrics) -> bool {
    metrics.input_tokens == 0 && metrics.output_tokens == 0 && metrics.total_tokens() == 0
}

fn u64_to_i64(value: u64, field: &str) -> anyhow::Result<i64> {
    i64::try_from(value).map_err(|_| anyhow::anyhow!("{field} exceeds i64::MAX"))
}

fn i64_to_u64(value: i64, field: &str) -> anyhow::Result<u64> {
    u64::try_from(value).map_err(|_| anyhow::anyhow!("{field} is negative"))
}

fn i64_to_u32(value: i64, field: &str) -> anyhow::Result<u32> {
    u32::try_from(value).map_err(|_| anyhow::anyhow!("{field} is outside u32 range"))
}
