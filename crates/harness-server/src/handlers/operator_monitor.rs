//! GET /api/operator-monitor — reconciled operator monitoring cockpit data.
//!
//! This endpoint is intentionally separate from `/api/overview`: overview keeps
//! the broad dashboard payload, while this payload answers current operator
//! questions across workflow runtime state, legacy task rows, failures, and
//! worktrees.

mod activity;
mod driverless_progress;
mod sampling;

use crate::http::AppState;
use crate::runtime_projection::{
    stopped_action_eligibility_for_workflows, RuntimeStoppedActionEligibility,
    RuntimeStoppedStateProjection, RuntimeWorkflowProjection,
};
use crate::task_runner::{RecentFailureTask, SchedulerAuthorityState, TaskSummary};
use activity::{runtime_workflow_counts, source_activity, RuntimeWorkflowCounts, SourceActivity};
use axum::{extract::State, http::StatusCode, Json};
use chrono::{DateTime, Utc};
use harness_workflow::runtime::{WorkflowInstance, WorkflowRuntimeStore};
use sampling::{dedupe_workflows, list_operator_action_workflows, list_recent_failed_workflows};
use serde::Serialize;
use serde_json::{json, Value};
use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use driverless_progress::{list_driverless_progress, DriverlessProgressEvidence};

const WORKFLOW_SAMPLE_LIMIT: i64 = 500;
const FAILED_WORKFLOW_SAMPLE_RESERVE: usize = 100;
const MAX_OPERATOR_ACTIONS: usize = 40;
const MAX_FAILURE_GROUPS: usize = 20;
const MAX_RECENT_FAILURES: i64 = 100;
const STALLED_AFTER_MINS: u64 = 30;
const OPERATOR_ACTION_STATES: &[&str] = &["ready_to_merge", "awaiting_feedback", "blocked"];

#[derive(Debug, Clone, Serialize)]
struct OperatorMonitorPayload {
    generated_at: String,
    sample_limit: i64,
    health: OperatorHealth,
    activity: OperatorActivity,
    operator_actions: Vec<OperatorAction>,
    stuck_workflows: Vec<StuckWorkflow>,
    driverless_progress: Vec<DriverlessProgressEvidence>,
    failures: Vec<FailureGroup>,
    worktrees: WorktreeSummary,
}

#[derive(Debug, Clone, Serialize)]
struct OperatorHealth {
    status: &'static str,
    degraded_subsystems: Vec<&'static str>,
    runtime_log_state: &'static str,
    runtime_log_path: Option<String>,
    uptime_secs: u64,
    runtime_hosts_online: u64,
    runtime_hosts_total: u64,
}

#[derive(Debug, Clone, Serialize)]
struct OperatorActivity {
    runtime_workflows: RuntimeWorkflowCounts,
    legacy_queue: LegacyQueueCounts,
    by_source: Vec<SourceActivity>,
    token_dispatch_by_repo: Vec<crate::http::GitHubTokenDispatchCounterSnapshot>,
}

#[derive(Debug, Default, Clone, Serialize, PartialEq, Eq)]
struct LegacyQueueCounts {
    queued: u64,
    running: u64,
    stalled: u64,
    failed: u64,
    done: u64,
}

#[derive(Debug, Clone, Serialize)]
struct OperatorAction {
    kind: &'static str,
    repo: Option<String>,
    issue: Option<u64>,
    pr: Option<u64>,
    task_id: Option<String>,
    workflow_id: String,
    state: String,
    age_secs: u64,
    url: Option<String>,
    evidence_url: Option<String>,
    next_action: &'static str,
    source: String,
    #[serde(flatten)]
    stopped_state: RuntimeStoppedStateProjection,
}

#[derive(Debug, Clone, Serialize)]
struct StuckWorkflow {
    workflow_id: String,
    definition_id: String,
    state: String,
    repo: Option<String>,
    issue: Option<u64>,
    pr: Option<u64>,
    age_secs: u64,
    updated_at: String,
    url: Option<String>,
    source: String,
    #[serde(flatten)]
    stopped_state: RuntimeStoppedStateProjection,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
struct FailureGroup {
    family: &'static str,
    severity: &'static str,
    message: String,
    first_seen: Option<String>,
    last_seen: Option<String>,
    count: u64,
    repo: Option<String>,
    task_id: Option<String>,
    retryable: bool,
    next_action: &'static str,
}

#[derive(Debug, Clone, Serialize)]
struct WorktreeSummary {
    used: u64,
    capacity: u64,
    stale: Option<u64>,
    metrics_state: &'static str,
    cards: Vec<crate::handlers::worktrees::WorktreeResponse>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct FailureGroupKey {
    family: &'static str,
    message: String,
    repo: Option<String>,
}

pub async fn operator_monitor(State(state): State<Arc<AppState>>) -> (StatusCode, Json<Value>) {
    match build_operator_monitor(&state).await {
        Ok(payload) => (
            StatusCode::OK,
            Json(serde_json::to_value(payload).unwrap_or_else(|error| {
                json!({ "error": format!("failed to serialize operator monitor: {error}") })
            })),
        ),
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": error.to_string() })),
        ),
    }
}

async fn build_operator_monitor(state: &AppState) -> anyhow::Result<OperatorMonitorPayload> {
    let generated_at = Utc::now();
    let workflows = list_runtime_workflows(state).await?;
    let active_tasks = state.core.tasks.list_active_summaries().await?;
    let stalled_tasks = state
        .core
        .tasks
        .list_stalled_tasks(Duration::from_secs(STALLED_AFTER_MINS * 60), None)
        .await?;
    let recent_failures = state
        .core
        .tasks
        .list_recent_failed(MAX_RECENT_FAILURES)
        .await?;
    let worktree_cards = crate::handlers::worktrees::list_worktrees(state).await?;

    let dashboard_counts = state.task_svc.count_for_dashboard().await;
    let runtime_hosts = state.runtime_hosts.list_hosts();
    let runtime_host_leases: u64 = runtime_hosts
        .iter()
        .map(|host| state.core.tasks.active_runtime_host_lease_count(&host.id) as u64)
        .sum();
    let runtime_log_state = state.core.server.runtime_logs.state.as_str();
    let degraded_subsystems = state.degraded_subsystems.clone();
    let runtime_state_dirty = state.is_runtime_state_dirty();
    let isolation_degraded = !state
        .isolation_availability
        .unavailable_required_tiers(&state.core.server.config.isolation)
        .is_empty();
    let health_status = if degraded_subsystems.is_empty()
        && runtime_log_state != "degraded"
        && !runtime_state_dirty
        && !isolation_degraded
    {
        "ok"
    } else {
        "degraded"
    };

    let runtime_workflows = runtime_workflow_counts(&workflows);
    let workflow_legacy_task_ids = workflow_legacy_task_ids(&workflows);
    let stopped_eligibility = stopped_action_eligibility_for_workflows(
        state.core.workflow_runtime_store.as_deref(),
        &workflows,
    )
    .await?;
    let stalled_task_count = stalled_tasks
        .iter()
        .filter(|task| !workflow_legacy_task_ids.contains(task.id.as_str()))
        .count() as u64;
    let active_tasks = filter_workflow_backed_tasks(active_tasks, &workflow_legacy_task_ids);
    let legacy_queue = legacy_queue_counts(
        &active_tasks,
        stalled_task_count,
        dashboard_counts.global_failed,
        dashboard_counts.global_done,
    );
    let by_source = source_activity(&workflows, &active_tasks);
    let operator_actions = operator_actions(&workflows, generated_at, &stopped_eligibility);
    let stuck_workflows = list_stuck_workflows(state, generated_at).await?;
    let driverless_progress = list_driverless_progress(state).await?;
    let failures = grouped_failures(&recent_failures, &workflows);
    let capacity = state.concurrency.task_queue.global_limit() as u64;
    let local_live_worktrees = state
        .concurrency
        .workspace_mgr
        .as_deref()
        .map(|manager| manager.live_count());

    Ok(OperatorMonitorPayload {
        generated_at: generated_at.to_rfc3339(),
        sample_limit: WORKFLOW_SAMPLE_LIMIT,
        health: OperatorHealth {
            status: health_status,
            degraded_subsystems,
            runtime_log_state,
            runtime_log_path: state
                .core
                .server
                .runtime_logs
                .active_path
                .as_ref()
                .map(|path| path.to_string_lossy().into_owned()),
            uptime_secs: crate::handlers::dashboard::SERVER_START
                .get()
                .map(|start| start.elapsed().as_secs())
                .unwrap_or(0),
            runtime_hosts_online: runtime_hosts.iter().filter(|host| host.online).count() as u64,
            runtime_hosts_total: runtime_hosts.len() as u64,
        },
        activity: OperatorActivity {
            runtime_workflows,
            legacy_queue,
            by_source,
            token_dispatch_by_repo: state.intake.github_token_dispatch_snapshot(),
        },
        operator_actions,
        stuck_workflows,
        driverless_progress,
        failures,
        worktrees: WorktreeSummary {
            used: worktree_used_count(local_live_worktrees, worktree_cards.len())
                .saturating_add(runtime_host_leases),
            capacity,
            stale: stale_worktree_count(local_live_worktrees, worktree_cards.len()),
            metrics_state: "unavailable",
            cards: worktree_cards,
        },
    })
}

async fn list_stuck_workflows(
    state: &AppState,
    generated_at: DateTime<Utc>,
) -> anyhow::Result<Vec<StuckWorkflow>> {
    let Some(store) = state.core.workflow_runtime_store.as_ref() else {
        return Ok(Vec::new());
    };
    let workflow_cfg =
        harness_core::config::workflow::load_workflow_config(&state.core.project_root)?;
    if !workflow_cfg.storage.workflow_watchdog_enabled {
        return Ok(Vec::new());
    }
    let cutoff = generated_at
        - chrono::Duration::minutes(workflow_cfg.storage.workflow_watchdog_age_minutes as i64);
    let workflows = store
        .list_aged_wait_instances(
            &["blocked", "awaiting_feedback"],
            cutoff,
            workflow_cfg.storage.workflow_watchdog_batch_size as i64,
        )
        .await?;
    let stopped_eligibility =
        stopped_action_eligibility_for_workflows(Some(store), &workflows).await?;
    Ok(stuck_workflows_from_instances(
        &workflows,
        generated_at,
        &stopped_eligibility,
    ))
}

fn stuck_workflows_from_instances(
    workflows: &[WorkflowInstance],
    generated_at: DateTime<Utc>,
    stopped_eligibility: &HashMap<String, RuntimeStoppedActionEligibility>,
) -> Vec<StuckWorkflow> {
    let mut stuck = workflows
        .iter()
        .map(|workflow| {
            let repo = string_field(&workflow.data, "repo");
            let issue = u64_field(&workflow.data, "issue_number");
            let pr = u64_field(&workflow.data, "pr_number");
            let pr_url = string_field(&workflow.data, "pr_url");
            let issue_url = repo
                .as_ref()
                .zip(issue)
                .map(|(repo, issue)| format!("https://github.com/{repo}/issues/{issue}"));
            StuckWorkflow {
                workflow_id: workflow.id.clone(),
                definition_id: workflow.definition_id.clone(),
                state: workflow.state.clone(),
                repo,
                issue,
                pr,
                age_secs: generated_at
                    .signed_duration_since(workflow.updated_at)
                    .num_seconds()
                    .max(0) as u64,
                updated_at: workflow.updated_at.to_rfc3339(),
                url: pr_url.or(issue_url),
                source: workflow_source(workflow),
                stopped_state: RuntimeStoppedStateProjection::from_workflow(workflow)
                    .with_action_eligibility(
                        stopped_eligibility
                            .get(&workflow.id)
                            .copied()
                            .unwrap_or_default(),
                    ),
            }
        })
        .collect::<Vec<_>>();
    stuck.sort_by_key(|workflow| std::cmp::Reverse(workflow.age_secs));
    stuck
}

async fn list_runtime_workflows(state: &AppState) -> anyhow::Result<Vec<WorkflowInstance>> {
    let Some(store) = state.core.workflow_runtime_store.as_ref() else {
        return Ok(Vec::new());
    };
    list_runtime_workflows_from_store(store).await
}

async fn list_runtime_workflows_from_store(
    store: &WorkflowRuntimeStore,
) -> anyhow::Result<Vec<WorkflowInstance>> {
    let sample_limit = WORKFLOW_SAMPLE_LIMIT as usize;
    let definition_ids = super::definition_ids::operator_definition_ids()?;
    let mut workflows = list_operator_action_workflows(store, &definition_ids).await?;
    workflows.extend(list_recent_failed_workflows(store, sample_limit, &definition_ids).await?);
    for definition_id in &definition_ids {
        workflows.extend(
            store
                .list_nonterminal_instances_by_definition(
                    definition_id,
                    None,
                    Some(WORKFLOW_SAMPLE_LIMIT),
                )
                .await?,
        );
    }
    dedupe_workflows(&mut workflows);
    truncate_workflow_sample(&mut workflows, sample_limit);
    Ok(workflows)
}

fn truncate_workflow_sample(workflows: &mut Vec<WorkflowInstance>, limit: usize) {
    let mut operator_actions = Vec::new();
    let mut failed = Vec::new();
    let mut other = Vec::new();
    for workflow in workflows.drain(..) {
        if workflow.state == "failed" {
            failed.push(workflow);
        } else if workflow_action_kind(workflow.state.as_str()).is_some() {
            operator_actions.push(workflow);
        } else {
            other.push(workflow);
        }
    }
    for group in [&mut operator_actions, &mut failed, &mut other] {
        group.sort_by_key(|workflow| Reverse(workflow.updated_at));
    }

    let mut selected = Vec::with_capacity(limit);
    let failed_reserve = failed.len().min(FAILED_WORKFLOW_SAMPLE_RESERVE).min(limit);
    selected.extend(failed.drain(..failed_reserve));

    let action_count = operator_actions
        .len()
        .min(limit.saturating_sub(selected.len()));
    selected.extend(operator_actions.drain(..action_count));

    let failed_count = failed.len().min(limit.saturating_sub(selected.len()));
    selected.extend(failed.drain(..failed_count));

    let other_count = other.len().min(limit.saturating_sub(selected.len()));
    selected.extend(other.drain(..other_count));

    selected.sort_by_key(|workflow| Reverse(workflow.updated_at));
    *workflows = selected;
}

fn legacy_queue_counts(
    active_tasks: &[TaskSummary],
    stalled: u64,
    failed: u64,
    done: u64,
) -> LegacyQueueCounts {
    let mut counts = LegacyQueueCounts {
        stalled,
        failed,
        done,
        ..Default::default()
    };
    for task in active_tasks {
        match task.scheduler.authority_state {
            SchedulerAuthorityState::Running
            | SchedulerAuthorityState::Leased
            | SchedulerAuthorityState::Recovering => counts.running += 1,
            _ => counts.queued += 1,
        }
    }
    counts
}

fn workflow_legacy_task_ids(workflows: &[WorkflowInstance]) -> HashSet<String> {
    workflows
        .iter()
        .filter_map(|workflow| {
            RuntimeWorkflowProjection::from_workflow(workflow)
                .legacy_dedupe_task_handle
                .map(|task_id| task_id.0)
        })
        .collect()
}

fn filter_workflow_backed_tasks(
    tasks: Vec<TaskSummary>,
    workflow_task_ids: &HashSet<String>,
) -> Vec<TaskSummary> {
    tasks
        .into_iter()
        .filter(|task| !workflow_task_ids.contains(task.id.as_str()))
        .collect()
}

fn operator_actions(
    workflows: &[WorkflowInstance],
    generated_at: DateTime<Utc>,
    stopped_eligibility: &HashMap<String, RuntimeStoppedActionEligibility>,
) -> Vec<OperatorAction> {
    let mut actions = Vec::new();
    for workflow in workflows {
        let Some(kind) = workflow_action_kind(workflow.state.as_str()) else {
            continue;
        };
        let projection = RuntimeWorkflowProjection::from_workflow_with_stopped_eligibility(
            workflow,
            stopped_eligibility
                .get(&workflow.id)
                .copied()
                .unwrap_or_default(),
        );
        let next_action = workflow_next_action(kind, &projection.stopped_state);
        let task_id = projection
            .legacy_dedupe_task_handle
            .as_ref()
            .map(|task_id| task_id.0.clone())
            .or_else(|| {
                projection
                    .submission_handle
                    .as_ref()
                    .map(|task_id| task_id.as_str().to_string())
            });
        let repo = string_field(&workflow.data, "repo");
        let issue = u64_field(&workflow.data, "issue_number");
        let pr = u64_field(&workflow.data, "pr_number");
        let pr_url = string_field(&workflow.data, "pr_url");
        let issue_url = repo
            .as_ref()
            .zip(issue)
            .map(|(repo, issue)| format!("https://github.com/{repo}/issues/{issue}"));
        actions.push(OperatorAction {
            kind,
            repo,
            issue,
            pr,
            evidence_url: task_id.as_ref().map(|id| format!("/tasks/{id}")),
            task_id,
            workflow_id: workflow.id.clone(),
            state: workflow.state.clone(),
            age_secs: generated_at
                .signed_duration_since(workflow.updated_at)
                .num_seconds()
                .max(0) as u64,
            url: pr_url.or(issue_url),
            next_action,
            source: workflow_source(workflow),
            stopped_state: projection.stopped_state,
        });
    }
    actions.sort_by(|a, b| {
        action_priority(a.kind)
            .cmp(&action_priority(b.kind))
            .then_with(|| b.age_secs.cmp(&a.age_secs))
    });
    actions.truncate(MAX_OPERATOR_ACTIONS);
    actions
}

fn workflow_action_kind(state: &str) -> Option<&'static str> {
    match state {
        "ready_to_merge" => Some("ready_to_merge"),
        "awaiting_feedback" => Some("awaiting_feedback"),
        "blocked" => Some("blocked"),
        "failed" => Some("failed"),
        _ => None,
    }
}

fn workflow_next_action(kind: &str, stopped_state: &RuntimeStoppedStateProjection) -> &'static str {
    match kind {
        "ready_to_merge" => "Review and merge",
        "awaiting_feedback" => "Inspect review feedback",
        "blocked" => "Resolve blocker",
        "failed" if stopped_state.can_retry => "Retry failed workflow",
        "failed" => "Inspect failed workflow",
        _ => "Inspect workflow",
    }
}

fn action_priority(kind: &str) -> u8 {
    match kind {
        "ready_to_merge" => 0,
        "blocked" => 1,
        "failed" => 2,
        "awaiting_feedback" => 3,
        _ => 3,
    }
}

fn grouped_failures(
    failures: &[RecentFailureTask],
    workflows: &[WorkflowInstance],
) -> Vec<FailureGroup> {
    let mut groups: HashMap<FailureGroupKey, FailureGroup> = HashMap::new();
    let mut represented_task_ids = HashSet::new();
    for failure in failures {
        represented_task_ids.insert(failure.id.as_str().to_string());
        let raw_message = failure.error.as_deref().unwrap_or("unknown failure");
        add_failure_group(
            &mut groups,
            raw_message,
            failure.failed_at.clone(),
            failure.project.as_deref().map(project_label),
            Some(failure.id.as_str().to_string()),
        );
    }
    for workflow in workflows
        .iter()
        .filter(|workflow| workflow.state == "failed")
    {
        let workflow_task_ids = workflow_failure_task_ids(workflow);
        if workflow_task_ids
            .iter()
            .any(|task_id| represented_task_ids.contains(task_id))
        {
            continue;
        }
        add_failure_group(
            &mut groups,
            &workflow_failure_message(workflow),
            Some(workflow.updated_at.to_rfc3339()),
            string_field(&workflow.data, "repo"),
            Some(workflow_failure_id(workflow)),
        );
    }
    let mut rows: Vec<FailureGroup> = groups.into_values().collect();
    rows.sort_by(|a, b| {
        b.last_seen
            .as_deref()
            .cmp(&a.last_seen.as_deref())
            .then_with(|| b.count.cmp(&a.count))
            .then_with(|| a.family.cmp(b.family))
    });
    rows.truncate(MAX_FAILURE_GROUPS);
    rows
}

fn add_failure_group(
    groups: &mut HashMap<FailureGroupKey, FailureGroup>,
    raw_message: &str,
    failed_at: Option<String>,
    repo: Option<String>,
    task_id: Option<String>,
) {
    let family = classify_failure_family(raw_message);
    let key = FailureGroupKey {
        family,
        message: normalize_failure_message(raw_message),
        repo,
    };
    let group = groups.entry(key.clone()).or_insert_with(|| FailureGroup {
        family,
        severity: failure_severity(family),
        message: key.message.clone(),
        first_seen: failed_at.clone(),
        last_seen: failed_at.clone(),
        count: 0,
        repo: key.repo.clone(),
        task_id: task_id.clone(),
        retryable: failure_retryable(family),
        next_action: failure_next_action(family),
    });
    group.count += 1;
    group.first_seen = earlier_timestamp(group.first_seen.as_deref(), failed_at.as_deref());
    group.last_seen = later_timestamp(group.last_seen.as_deref(), failed_at.as_deref());
    if group.task_id.is_none() {
        if let Some(task_id) = task_id {
            group.task_id = Some(task_id);
        }
    }
}

fn workflow_failure_message(workflow: &WorkflowInstance) -> String {
    ["failure_reason", "previous_error", "last_error", "error"]
        .into_iter()
        .find_map(|field| string_field(&workflow.data, field))
        .unwrap_or_else(|| format!("{} workflow failed", workflow.definition_id))
}

fn workflow_failure_id(workflow: &WorkflowInstance) -> String {
    let projection = RuntimeWorkflowProjection::from_workflow(workflow);
    projection
        .submission_handle
        .map(|task_id| task_id.as_str().to_string())
        .or_else(|| {
            projection
                .legacy_dedupe_task_handle
                .map(|task_id| task_id.0)
        })
        .unwrap_or_else(|| workflow.id.clone())
}

fn workflow_failure_task_ids(workflow: &WorkflowInstance) -> HashSet<String> {
    let projection = RuntimeWorkflowProjection::from_workflow(workflow);
    let mut ids = HashSet::new();
    if let Some(task_id) = projection.submission_handle {
        ids.insert(task_id.as_str().to_string());
    }
    if let Some(task_id) = projection.legacy_dedupe_task_handle {
        ids.insert(task_id.0);
    }
    ids
}

fn classify_failure_family(message: &str) -> &'static str {
    let lower = message.to_ascii_lowercase();
    if lower.contains("ssl_error_syscall")
        || lower.contains("failed to fetch")
        || lower.contains("git fetch")
        || lower.contains("github.com")
    {
        "github_fetch"
    } else if lower.contains("timed out") || lower.contains("timeout") {
        "timeout"
    } else if lower.contains("rate limit") || lower.contains("secondary rate limit") {
        "rate_limit"
    } else if lower.contains("missing structured output")
        || lower.contains("activity_result")
        || lower.contains("structured activity")
    {
        "missing_structured_output"
    } else if lower.contains("worktree") || lower.contains("workspace") {
        "worktree_collision"
    } else if lower.contains("agent turn") || lower.contains("agent failed") {
        "agent_turn_failed"
    } else {
        "internal"
    }
}

fn failure_severity(family: &str) -> &'static str {
    match family {
        "github_fetch" | "timeout" | "rate_limit" => "warn",
        "missing_structured_output" | "worktree_collision" | "agent_turn_failed" => "error",
        _ => "error",
    }
}

fn failure_retryable(family: &str) -> bool {
    matches!(family, "github_fetch" | "timeout" | "rate_limit")
}

fn failure_next_action(family: &str) -> &'static str {
    match family {
        "github_fetch" => "Retry after GitHub connectivity recovers",
        "timeout" => "Retry or inspect the long-running turn",
        "rate_limit" => "Wait for the rate limit window",
        "missing_structured_output" => "Inspect agent output and prompt contract",
        "worktree_collision" => "Inspect workspace ownership",
        "agent_turn_failed" => "Inspect agent logs",
        _ => "Inspect task logs",
    }
}

fn normalize_failure_message(message: &str) -> String {
    let first_line = message.lines().next().unwrap_or(message).trim();
    let collapsed = first_line.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() > 180 {
        collapsed.chars().take(177).collect::<String>() + "..."
    } else if collapsed.is_empty() {
        "unknown failure".to_string()
    } else {
        collapsed
    }
}

fn earlier_timestamp(current: Option<&str>, candidate: Option<&str>) -> Option<String> {
    current
        .into_iter()
        .chain(candidate)
        .min()
        .map(str::to_string)
}

fn later_timestamp(current: Option<&str>, candidate: Option<&str>) -> Option<String> {
    current
        .into_iter()
        .chain(candidate)
        .max()
        .map(str::to_string)
}

fn worktree_used_count(local_live_count: Option<u64>, card_count: usize) -> u64 {
    local_live_count.unwrap_or(card_count as u64)
}

fn stale_worktree_count(local_live_count: Option<u64>, card_count: usize) -> Option<u64> {
    local_live_count.map(|count| count.saturating_sub(card_count as u64))
}

fn workflow_source(workflow: &WorkflowInstance) -> String {
    string_field(&workflow.data, "source")
        .filter(|source| !source.trim().is_empty())
        .unwrap_or_else(|| match workflow.definition_id.as_str() {
            harness_workflow::runtime::GITHUB_ISSUE_PR_DEFINITION_ID => "github".to_string(),
            harness_workflow::runtime::PR_FEEDBACK_DEFINITION_ID => {
                "operator_pr_feedback".to_string()
            }
            harness_workflow::runtime::PROMPT_TASK_DEFINITION_ID => "manual".to_string(),
            harness_workflow::runtime::QUALITY_GATE_DEFINITION_ID => "quality_gate".to_string(),
            _ => "workflow_runtime".to_string(),
        })
}

fn string_field(data: &Value, field: &str) -> Option<String> {
    data.get(field)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn u64_field(data: &Value, field: &str) -> Option<u64> {
    data.get(field)
        .and_then(|value| value.as_u64().or_else(|| value.as_str()?.parse().ok()))
}

fn project_label(path: &str) -> String {
    std::path::Path::new(path)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string())
}

#[cfg(test)]
mod declarative_visibility_tests;
#[cfg(test)]
mod tests;
