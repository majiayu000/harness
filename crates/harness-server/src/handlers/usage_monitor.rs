use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use chrono::{DateTime, Duration, Utc};
use harness_core::types::{Event, EventFilters};
use harness_observe::usage::UsageMetrics;
use harness_workflow::runtime::{
    RuntimeJob, RuntimeJobStatus, RuntimeKind, RuntimeProfile, WorkflowCommand, WorkflowInstance,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, OnceLock};

use crate::http::AppState;

#[path = "usage_monitor_local_usage.rs"]
mod usage_monitor_local_usage;
#[path = "usage_monitor_process.rs"]
mod usage_monitor_process;

use usage_monitor_local_usage::{load_local_usage_summaries, LocalUsageSourceSummary};
use usage_monitor_process::{sample_agent_processes, AgentProcess};

const DEFAULT_USAGE_WINDOW_HOURS: i64 = 24;
const MAX_USAGE_WINDOW_HOURS: i64 = 24 * 14;
const DEFAULT_RUNTIME_JOB_LIMIT: i64 = 200;

#[derive(Debug, Deserialize)]
pub(crate) struct UsageMonitorQuery {
    hours: Option<i64>,
    limit: Option<i64>,
}

#[derive(Debug, Serialize)]
struct UsageMonitorResponse {
    window: UsageWindow,
    cost: CostConfig,
    summary: UsageSummary,
    tokens_by_agent: Vec<UsageGroup>,
    tokens_by_project: Vec<UsageGroup>,
    tokens_by_model: Vec<UsageGroup>,
    agent_invocations: Vec<AgentInvocation>,
    external_agent_processes: Vec<AgentProcess>,
    local_usage_sources: Vec<LocalUsageSourceSummary>,
    active_by_repo: Vec<ActiveCount>,
    active_by_activity: Vec<ActiveCount>,
    diagnostics: UsageDiagnostics,
}

#[derive(Debug, Clone, Copy, Serialize)]
struct UsageWindow {
    hours: i64,
    since: DateTime<Utc>,
    now: DateTime<Utc>,
}

impl UsageWindow {
    fn from_query(hours: Option<i64>, now: DateTime<Utc>) -> Self {
        let hours = hours
            .unwrap_or(DEFAULT_USAGE_WINDOW_HOURS)
            .clamp(1, MAX_USAGE_WINDOW_HOURS);
        Self {
            hours,
            since: now - Duration::hours(hours),
            now,
        }
    }
}

#[derive(Debug, Serialize)]
struct CostConfig {
    currency: &'static str,
    configured: bool,
    source: &'static str,
    missing_model_count: usize,
    message: &'static str,
}

#[derive(Debug, Serialize)]
struct UsageSummary {
    total_tokens: u64,
    input_tokens: u64,
    output_tokens: u64,
    cache_read_input_tokens: u64,
    cache_creation_input_tokens: u64,
    request_count: u64,
    estimated_cost_usd: Option<f64>,
    running_agent_invocations: u64,
    pending_agent_invocations: u64,
    stale_agent_invocations: u64,
    high_burn_invocations: u64,
    external_agent_processes: u64,
}

#[derive(Debug, Default, Clone)]
struct UsageAggregate {
    input_tokens: u64,
    output_tokens: u64,
    cache_read_input_tokens: u64,
    cache_creation_input_tokens: u64,
    request_count: u64,
    estimated_cost_usd: f64,
    missing_price: bool,
}

impl UsageAggregate {
    fn add(&mut self, metrics: &UsageMetrics, estimated_cost_usd: Option<f64>) {
        self.input_tokens = self.input_tokens.saturating_add(metrics.input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(metrics.output_tokens);
        self.cache_read_input_tokens = self
            .cache_read_input_tokens
            .saturating_add(metrics.cache_read_input_tokens);
        self.cache_creation_input_tokens = self
            .cache_creation_input_tokens
            .saturating_add(metrics.cache_creation_input_tokens);
        self.request_count = self.request_count.saturating_add(1);
        match estimated_cost_usd {
            Some(cost) => self.estimated_cost_usd += cost,
            None => self.missing_price = true,
        }
    }

    fn total_tokens(&self) -> u64 {
        self.input_tokens
            .saturating_add(self.output_tokens)
            .saturating_add(self.cache_read_input_tokens)
            .saturating_add(self.cache_creation_input_tokens)
    }

    fn estimated_cost_json(&self, prices_configured: bool) -> Option<f64> {
        if prices_configured && !self.missing_price {
            Some(round_cost(self.estimated_cost_usd))
        } else {
            None
        }
    }
}

#[derive(Debug, Serialize)]
struct UsageGroup {
    name: String,
    input_tokens: u64,
    output_tokens: u64,
    cache_read_input_tokens: u64,
    cache_creation_input_tokens: u64,
    total_tokens: u64,
    request_count: u64,
    estimated_cost_usd: Option<f64>,
    cost_confidence: &'static str,
}

#[derive(Debug)]
struct UsageRecord {
    agent: String,
    model: String,
    project: String,
    metrics: UsageMetrics,
    estimated_cost_usd: Option<f64>,
}

#[derive(Debug, Deserialize)]
struct ModelPrice {
    input_per_million_usd: f64,
    output_per_million_usd: f64,
    #[serde(default)]
    cache_read_per_million_usd: f64,
    #[serde(default)]
    cache_creation_per_million_usd: f64,
}

#[derive(Debug, Default)]
struct PriceCatalog {
    by_model: BTreeMap<String, ModelPrice>,
}

impl PriceCatalog {
    fn from_env_cached() -> &'static Self {
        static PRICE_CATALOG: OnceLock<PriceCatalog> = OnceLock::new();
        PRICE_CATALOG.get_or_init(Self::from_env)
    }

    fn from_env() -> Self {
        let Ok(raw) = std::env::var("HARNESS_USAGE_PRICE_CATALOG_JSON") else {
            return Self::default();
        };
        let raw = raw.trim();
        if raw.is_empty() {
            return Self::default();
        }
        match serde_json::from_str::<BTreeMap<String, ModelPrice>>(raw) {
            Ok(by_model) => Self { by_model },
            Err(error) => {
                tracing::warn!("usage_monitor: invalid HARNESS_USAGE_PRICE_CATALOG_JSON: {error}");
                Self::default()
            }
        }
    }

    fn configured(&self) -> bool {
        !self.by_model.is_empty()
    }

    fn estimate(&self, model: &str, metrics: &UsageMetrics) -> Option<f64> {
        let price = self.by_model.get(model)?;
        Some(
            (metrics.input_tokens as f64 * price.input_per_million_usd
                + metrics.output_tokens as f64 * price.output_per_million_usd
                + metrics.cache_read_input_tokens as f64 * price.cache_read_per_million_usd
                + metrics.cache_creation_input_tokens as f64
                    * price.cache_creation_per_million_usd)
                / 1_000_000.0,
        )
    }
}

#[derive(Debug, Serialize)]
struct AgentInvocation {
    agent_invocation_id: String,
    source: &'static str,
    runtime_job_id: String,
    command_id: String,
    workflow_id: String,
    workflow_definition: String,
    workflow_state: String,
    subject_type: String,
    subject_key: String,
    repo: Option<String>,
    project: Option<String>,
    issue_number: Option<u64>,
    pr_number: Option<u64>,
    task_id: Option<String>,
    activity: String,
    status: &'static str,
    command_status: String,
    agent_runtime: &'static str,
    runtime_profile: String,
    model: Option<String>,
    reasoning_effort: Option<String>,
    lease_owner: Option<String>,
    lease_expires_at: Option<DateTime<Utc>>,
    stale: bool,
    age_secs: i64,
    updated_age_secs: i64,
    burn_level: &'static str,
    cost_confidence: &'static str,
}

#[derive(Debug)]
struct RuntimeUsageRow {
    workflow: WorkflowInstance,
    command_id: String,
    command_status: String,
    command: WorkflowCommand,
    runtime_job: RuntimeJob,
}

#[derive(Debug, Serialize)]
struct ActiveCount {
    name: String,
    running: u64,
    pending: u64,
    high_burn: u64,
}

#[derive(Debug, Serialize)]
struct UsageDiagnostics {
    runtime_store_available: bool,
    token_source: &'static str,
    active_cost_confidence: &'static str,
    process_source: &'static str,
}

pub(crate) async fn usage_monitor(
    State(state): State<Arc<AppState>>,
    Query(query): Query<UsageMonitorQuery>,
) -> Response {
    match build_usage_monitor_response(&state, query).await {
        Ok(body) => (StatusCode::OK, Json(body)).into_response(),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({
                "error": "usage_monitor_unavailable",
                "message": error.to_string(),
            })),
        )
            .into_response(),
    }
}

async fn build_usage_monitor_response(
    state: &AppState,
    query: UsageMonitorQuery,
) -> anyhow::Result<UsageMonitorResponse> {
    let now = Utc::now();
    let window = UsageWindow::from_query(query.hours, now);
    let price_catalog = PriceCatalog::from_env_cached();
    let usage_records = load_usage_records(state, window, price_catalog).await?;
    let runtime_rows = load_runtime_usage_rows(state, window.since, query.limit).await?;
    let agent_invocations = runtime_rows
        .iter()
        .map(|row| agent_invocation_from_row(row, now))
        .collect::<Vec<_>>();
    let external_agent_processes =
        sample_agent_processes(now, &runtime_attribution_tokens(&runtime_rows));
    let local_usage_sources = load_local_usage_summaries(window).await;

    let tokens_by_agent =
        aggregate_usage(&usage_records, |record| record.agent.clone(), price_catalog);
    let tokens_by_project = aggregate_usage(
        &usage_records,
        |record| blank_label(&record.project),
        price_catalog,
    );
    let tokens_by_model =
        aggregate_usage(&usage_records, |record| record.model.clone(), price_catalog);
    let total_usage = total_usage_aggregate(&usage_records);

    let running_agent_invocations = agent_invocations
        .iter()
        .filter(|invocation| invocation.status == "running")
        .count() as u64;
    let pending_agent_invocations = agent_invocations
        .iter()
        .filter(|invocation| invocation.status == "pending")
        .count() as u64;
    let stale_agent_invocations = agent_invocations
        .iter()
        .filter(|invocation| invocation.stale)
        .count() as u64;
    let high_burn_invocations = agent_invocations
        .iter()
        .filter(|invocation| invocation.burn_level == "high")
        .count() as u64;

    let cost = CostConfig {
        currency: "USD",
        configured: price_catalog.configured(),
        source: "HARNESS_USAGE_PRICE_CATALOG_JSON",
        missing_model_count: usage_records
            .iter()
            .filter(|record| record.estimated_cost_usd.is_none())
            .map(|record| record.model.as_str())
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        message: if price_catalog.configured() {
            "Harness-attributed token costs use the configured local price catalog. ccstats local source totals are reported separately from workflow and task attribution."
        } else {
            "Harness-attributed token costs need a local price catalog. ccstats local source totals may still report global Codex and Claude costs separately."
        },
    };

    Ok(UsageMonitorResponse {
        window,
        cost,
        summary: UsageSummary {
            total_tokens: total_usage.total_tokens(),
            input_tokens: total_usage.input_tokens,
            output_tokens: total_usage.output_tokens,
            cache_read_input_tokens: total_usage.cache_read_input_tokens,
            cache_creation_input_tokens: total_usage.cache_creation_input_tokens,
            request_count: total_usage.request_count,
            estimated_cost_usd: total_usage.estimated_cost_json(price_catalog.configured()),
            running_agent_invocations,
            pending_agent_invocations,
            stale_agent_invocations,
            high_burn_invocations,
            external_agent_processes: external_agent_processes.len() as u64,
        },
        tokens_by_agent,
        tokens_by_project,
        tokens_by_model,
        active_by_repo: aggregate_active_counts(&agent_invocations, |invocation| {
            invocation
                .repo
                .clone()
                .unwrap_or_else(|| "unassigned".to_string())
        }),
        active_by_activity: aggregate_active_counts(&agent_invocations, |invocation| {
            invocation.activity.clone()
        }),
        agent_invocations,
        external_agent_processes,
        local_usage_sources,
        diagnostics: UsageDiagnostics {
            runtime_store_available: state.core.workflow_runtime_store.is_some(),
            token_source: "llm_usage_events",
            active_cost_confidence: "estimated_from_agent_invocation_state",
            process_source: "external_cli_process_snapshot",
        },
    })
}

async fn load_usage_records(
    state: &AppState,
    window: UsageWindow,
    price_catalog: &PriceCatalog,
) -> anyhow::Result<Vec<UsageRecord>> {
    let events = state
        .observability
        .events
        .query(&EventFilters {
            hook: Some("llm_usage".to_string()),
            since: Some(window.since),
            until: Some(window.now),
            include_content: true,
            ..Default::default()
        })
        .await?;
    Ok(events
        .iter()
        .filter_map(|event| parse_usage_event(event, price_catalog))
        .collect())
}

fn parse_usage_event(event: &Event, price_catalog: &PriceCatalog) -> Option<UsageRecord> {
    let content = event.content.as_deref()?;
    let payload: Value = serde_json::from_str(content).ok()?;
    let metrics = UsageMetrics::from_payload(&payload)?;
    let agent = payload
        .get("agent")
        .and_then(Value::as_str)
        .unwrap_or(event.tool.as_str())
        .to_string();
    let model = payload
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_string();
    let project = payload
        .get("project")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let estimated_cost_usd = price_catalog.estimate(&model, &metrics);
    Some(UsageRecord {
        agent,
        model,
        project,
        metrics,
        estimated_cost_usd,
    })
}

async fn load_runtime_usage_rows(
    state: &AppState,
    recent_since: DateTime<Utc>,
    limit: Option<i64>,
) -> anyhow::Result<Vec<RuntimeUsageRow>> {
    let Some(store) = state.core.workflow_runtime_store.as_ref() else {
        return Ok(Vec::new());
    };
    let limit = limit
        .unwrap_or(DEFAULT_RUNTIME_JOB_LIMIT)
        .clamp(1, DEFAULT_RUNTIME_JOB_LIMIT);
    let rows: Vec<(String, String, String, String, String, String)> = sqlx::query_as(
        "SELECT
             workflow.id,
             workflow.data::text,
             command.id,
             command.status,
             command.data::text,
             job.data::text
         FROM runtime_jobs job
         JOIN workflow_commands command ON command.id = job.command_id
         JOIN workflow_instances workflow ON workflow.id = command.workflow_id
         WHERE job.status IN ('pending', 'running')
            OR job.updated_at >= $1
         ORDER BY
             CASE job.status
                 WHEN 'running' THEN 0
                 WHEN 'pending' THEN 1
                 ELSE 2
             END ASC,
             job.updated_at DESC,
             job.id DESC
         LIMIT $2",
    )
    .bind(recent_since)
    .bind(limit)
    .fetch_all(store.pool())
    .await?;

    Ok(rows
        .into_iter()
        .filter_map(
            |(workflow_id, workflow, command_id, command_status, command, runtime_job)| {
                match parse_runtime_usage_row(
                    &workflow_id,
                    command_id,
                    command_status,
                    &workflow,
                    &command,
                    &runtime_job,
                ) {
                    Ok(row) => Some(row),
                    Err(error) => {
                        tracing::warn!(
                            workflow_id = %workflow_id,
                            "usage_monitor: skipping invalid runtime row: {error}"
                        );
                        None
                    }
                }
            },
        )
        .collect())
}

fn parse_runtime_usage_row(
    workflow_id: &str,
    command_id: String,
    command_status: String,
    workflow: &str,
    command: &str,
    runtime_job: &str,
) -> anyhow::Result<RuntimeUsageRow> {
    Ok(RuntimeUsageRow {
        workflow: serde_json::from_str(workflow).map_err(|error| {
            anyhow::anyhow!("workflow `{workflow_id}` JSON is invalid: {error}")
        })?,
        command_id: command_id.clone(),
        command_status,
        command: serde_json::from_str(command)
            .map_err(|error| anyhow::anyhow!("command `{command_id}` JSON is invalid: {error}"))?,
        runtime_job: serde_json::from_str(runtime_job).map_err(|error| {
            anyhow::anyhow!("runtime job for command `{command_id}` JSON is invalid: {error}")
        })?,
    })
}

fn runtime_attribution_tokens(rows: &[RuntimeUsageRow]) -> BTreeSet<String> {
    let mut tokens = BTreeSet::new();
    for row in rows {
        tokens.insert(row.command_id.clone());
        tokens.insert(row.runtime_job.id.clone());
    }
    tokens
}

fn agent_invocation_from_row(row: &RuntimeUsageRow, now: DateTime<Utc>) -> AgentInvocation {
    let job = &row.runtime_job;
    let workflow = &row.workflow;
    let runtime_profile = runtime_profile_from_job(job);
    let activity = job
        .input
        .get("activity")
        .and_then(Value::as_str)
        .or_else(|| row.command.activity_name())
        .unwrap_or_else(|| row.command.command_type.as_str())
        .to_string();
    let lease_expires_at = job.lease.as_ref().map(|lease| lease.expires_at);
    let stale = matches!(job.status, RuntimeJobStatus::Running)
        && lease_expires_at.is_some_and(|expires_at| expires_at <= now);
    let age_secs = (now - job.created_at).num_seconds().max(0);
    let updated_age_secs = (now - job.updated_at).num_seconds().max(0);
    let reasoning_effort = runtime_profile
        .as_ref()
        .and_then(|profile| profile.reasoning_effort.clone());
    let status = runtime_job_status(job.status);
    let burn_level = burn_level(
        status,
        &activity,
        reasoning_effort.as_deref(),
        age_secs,
        stale,
    );

    AgentInvocation {
        agent_invocation_id: job.id.clone(),
        source: "workflow_runtime",
        runtime_job_id: job.id.clone(),
        command_id: row.command_id.clone(),
        workflow_id: workflow.id.clone(),
        workflow_definition: workflow.definition_id.clone(),
        workflow_state: workflow.state.clone(),
        subject_type: workflow.subject.subject_type.clone(),
        subject_key: workflow.subject.subject_key.clone(),
        repo: string_field(&workflow.data, "repo").or_else(|| string_field(&job.input, "repo")),
        project: string_field(&workflow.data, "project_id")
            .or_else(|| string_field(&job.input, "project_id")),
        issue_number: u64_field(&workflow.data, "issue_number"),
        pr_number: u64_field(&workflow.data, "pr_number"),
        task_id: string_field(&workflow.data, "task_id")
            .or_else(|| string_field(&job.input, "task_id")),
        activity,
        status,
        command_status: row.command_status.clone(),
        agent_runtime: runtime_kind(job.runtime_kind),
        runtime_profile: job.runtime_profile.clone(),
        model: runtime_profile
            .as_ref()
            .and_then(|profile| profile.model.clone()),
        reasoning_effort,
        lease_owner: job.lease.as_ref().map(|lease| lease.owner.clone()),
        lease_expires_at,
        stale,
        age_secs,
        updated_age_secs,
        burn_level,
        cost_confidence: "estimated_runtime_burn",
    }
}

fn runtime_profile_from_job(job: &RuntimeJob) -> Option<RuntimeProfile> {
    job.input
        .get("runtime_profile")
        .and_then(|value| serde_json::from_value(value.clone()).ok())
}

fn aggregate_usage<F>(
    records: &[UsageRecord],
    key_fn: F,
    price_catalog: &PriceCatalog,
) -> Vec<UsageGroup>
where
    F: Fn(&UsageRecord) -> String,
{
    let mut groups: BTreeMap<String, UsageAggregate> = BTreeMap::new();
    for record in records {
        groups
            .entry(key_fn(record))
            .or_default()
            .add(&record.metrics, record.estimated_cost_usd);
    }
    let mut rows = groups
        .into_iter()
        .map(|(name, usage)| usage_group(name, usage, price_catalog.configured()))
        .collect::<Vec<_>>();
    rows.sort_by(|a, b| {
        b.total_tokens
            .cmp(&a.total_tokens)
            .then_with(|| a.name.cmp(&b.name))
    });
    rows
}

fn total_usage_aggregate(records: &[UsageRecord]) -> UsageAggregate {
    let mut usage = UsageAggregate::default();
    for record in records {
        usage.add(&record.metrics, record.estimated_cost_usd);
    }
    usage
}

fn usage_group(name: String, usage: UsageAggregate, prices_configured: bool) -> UsageGroup {
    UsageGroup {
        name,
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
        cache_read_input_tokens: usage.cache_read_input_tokens,
        cache_creation_input_tokens: usage.cache_creation_input_tokens,
        total_tokens: usage.total_tokens(),
        request_count: usage.request_count,
        estimated_cost_usd: usage.estimated_cost_json(prices_configured),
        cost_confidence: if prices_configured && !usage.missing_price {
            "exact_tokens_estimated_price"
        } else {
            "price_unavailable"
        },
    }
}

fn aggregate_active_counts<F>(invocations: &[AgentInvocation], key_fn: F) -> Vec<ActiveCount>
where
    F: Fn(&AgentInvocation) -> String,
{
    let mut counts: BTreeMap<String, ActiveCount> = BTreeMap::new();
    for invocation in invocations {
        let entry = counts
            .entry(key_fn(invocation))
            .or_insert_with_key(|name| ActiveCount {
                name: name.clone(),
                running: 0,
                pending: 0,
                high_burn: 0,
            });
        match invocation.status {
            "running" => entry.running += 1,
            "pending" => entry.pending += 1,
            _ => {}
        }
        if invocation.burn_level == "high" {
            entry.high_burn += 1;
        }
    }
    let mut rows = counts.into_values().collect::<Vec<_>>();
    rows.sort_by(|a, b| {
        let a_total = a.running + a.pending;
        let b_total = b.running + b.pending;
        b_total.cmp(&a_total).then_with(|| a.name.cmp(&b.name))
    });
    rows
}

fn burn_level(
    status: &str,
    activity: &str,
    reasoning_effort: Option<&str>,
    age_secs: i64,
    stale: bool,
) -> &'static str {
    if stale {
        return "high";
    }
    if status != "running" {
        return "low";
    }
    if matches!(reasoning_effort, Some("xhigh" | "high")) || age_secs >= 1800 {
        return "high";
    }
    if activity == "poll_repo_backlog" && age_secs >= 300 {
        return "medium";
    }
    if age_secs >= 900 {
        return "medium";
    }
    "low"
}

fn string_field(value: &Value, field: &str) -> Option<String> {
    value
        .get(field)
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
}

fn u64_field(value: &Value, field: &str) -> Option<u64> {
    value.get(field).and_then(Value::as_u64)
}

fn runtime_job_status(status: RuntimeJobStatus) -> &'static str {
    match status {
        RuntimeJobStatus::Pending => "pending",
        RuntimeJobStatus::Running => "running",
        RuntimeJobStatus::Succeeded => "succeeded",
        RuntimeJobStatus::Failed => "failed",
        RuntimeJobStatus::Cancelled => "cancelled",
        RuntimeJobStatus::Expired => "expired",
    }
}

fn runtime_kind(kind: RuntimeKind) -> &'static str {
    match kind {
        RuntimeKind::CodexExec => "codex_exec",
        RuntimeKind::CodexJsonrpc => "codex_jsonrpc",
        RuntimeKind::ClaudeCode => "claude_code",
        RuntimeKind::AnthropicApi => "anthropic_api",
        RuntimeKind::RemoteHost => "remote_host",
    }
}

fn blank_label(value: &str) -> String {
    if value.trim().is_empty() {
        "unassigned".to_string()
    } else {
        value.to_string()
    }
}

fn round_cost(value: f64) -> f64 {
    (value * 10_000.0).round() / 10_000.0
}

#[cfg(test)]
#[path = "usage_monitor_tests.rs"]
mod tests;
