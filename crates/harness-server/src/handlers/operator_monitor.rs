//! GET /api/operator-monitor — reconciled operator monitoring cockpit data.
//!
//! This endpoint is intentionally separate from `/api/overview`: overview keeps
//! the broad dashboard payload, while this payload answers current operator
//! questions across workflow runtime state, legacy task rows, failures, and
//! worktrees.

use crate::http::AppState;
use crate::runtime_projection::{RuntimeActiveBucket, RuntimeWorkflowProjection};
use crate::task_runner::{RecentFailureTask, SchedulerAuthorityState, TaskSummary};
use crate::workspace::WorkspaceManager;
use axum::{extract::State, http::StatusCode, Json};
use chrono::{DateTime, Utc};
use harness_workflow::runtime::WorkflowInstance;
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

const WORKFLOW_SAMPLE_LIMIT: i64 = 500;
const MAX_OPERATOR_ACTIONS: usize = 40;
const MAX_FAILURE_GROUPS: usize = 20;
const MAX_RECENT_FAILURES: i64 = 100;
const STALLED_AFTER_MINS: u64 = 30;

#[derive(Debug, Clone, Serialize)]
struct OperatorMonitorPayload {
    generated_at: String,
    sample_limit: i64,
    health: OperatorHealth,
    activity: OperatorActivity,
    operator_actions: Vec<OperatorAction>,
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
}

#[derive(Debug, Default, Clone, Serialize, PartialEq, Eq)]
struct RuntimeWorkflowCounts {
    pending: u64,
    running: u64,
    review: u64,
    awaiting_dependencies: u64,
    ready_to_merge: u64,
    blocked: u64,
    failed: u64,
    done: u64,
    other: u64,
}

#[derive(Debug, Default, Clone, Serialize, PartialEq, Eq)]
struct LegacyQueueCounts {
    queued: u64,
    running: u64,
    stalled: u64,
    failed: u64,
    done: u64,
}

#[derive(Debug, Default, Clone, Serialize, PartialEq, Eq)]
struct SourceActivity {
    source: String,
    running: u64,
    blocked: u64,
    failed: u64,
    ready_to_merge: u64,
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
    let runtime_log_state = state.core.server.runtime_logs.state.as_str();
    let degraded_subsystems = state.degraded_subsystems.clone();
    let health_status = if degraded_subsystems.is_empty() && runtime_log_state != "degraded" {
        "ok"
    } else {
        "degraded"
    };

    let runtime_workflows = runtime_workflow_counts(&workflows);
    let legacy_queue = legacy_queue_counts(
        &active_tasks,
        stalled_tasks.len() as u64,
        dashboard_counts.global_failed,
        dashboard_counts.global_done,
    );
    let by_source = source_activity(&workflows, &active_tasks);
    let operator_actions = operator_actions(&workflows, generated_at);
    let failures = grouped_failures(&recent_failures);
    let capacity = state.concurrency.task_queue.global_limit() as u64;

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
        },
        operator_actions,
        failures,
        worktrees: WorktreeSummary {
            used: worktree_cards.len() as u64,
            capacity,
            stale: stale_worktree_count(
                state.concurrency.workspace_mgr.as_deref(),
                &worktree_cards,
            ),
            metrics_state: "unavailable",
            cards: worktree_cards,
        },
    })
}

async fn list_runtime_workflows(state: &AppState) -> anyhow::Result<Vec<WorkflowInstance>> {
    let Some(store) = state.core.workflow_runtime_store.as_ref() else {
        return Ok(Vec::new());
    };
    store.list_instances(None, WORKFLOW_SAMPLE_LIMIT).await
}

fn runtime_workflow_counts(workflows: &[WorkflowInstance]) -> RuntimeWorkflowCounts {
    let mut counts = RuntimeWorkflowCounts::default();
    for workflow in workflows {
        match workflow_bucket(workflow) {
            WorkflowBucket::Pending => counts.pending += 1,
            WorkflowBucket::Running => counts.running += 1,
            WorkflowBucket::Review => counts.review += 1,
            WorkflowBucket::AwaitingDependencies => counts.awaiting_dependencies += 1,
            WorkflowBucket::ReadyToMerge => counts.ready_to_merge += 1,
            WorkflowBucket::Blocked => counts.blocked += 1,
            WorkflowBucket::Failed => counts.failed += 1,
            WorkflowBucket::Done => counts.done += 1,
            WorkflowBucket::Other => counts.other += 1,
        }
    }
    counts
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WorkflowBucket {
    Pending,
    Running,
    Review,
    AwaitingDependencies,
    ReadyToMerge,
    Blocked,
    Failed,
    Done,
    Other,
}

fn workflow_bucket(workflow: &WorkflowInstance) -> WorkflowBucket {
    match workflow.state.as_str() {
        "scheduled" | "discovered" => WorkflowBucket::Pending,
        "awaiting_dependencies" => WorkflowBucket::AwaitingDependencies,
        "ready_to_merge" => WorkflowBucket::ReadyToMerge,
        "blocked" => WorkflowBucket::Blocked,
        "failed" => WorkflowBucket::Failed,
        "done" | "passed" => WorkflowBucket::Done,
        "pr_open" | "local_review_gate" | "awaiting_feedback" | "quality_gate_pending" => {
            WorkflowBucket::Review
        }
        _ => {
            let projection = RuntimeWorkflowProjection::from_workflow(workflow);
            match projection.active_bucket() {
                Some(RuntimeActiveBucket::Running) => WorkflowBucket::Running,
                Some(RuntimeActiveBucket::Queued) => WorkflowBucket::Pending,
                None => WorkflowBucket::Other,
            }
        }
    }
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

fn source_activity(
    workflows: &[WorkflowInstance],
    active_tasks: &[TaskSummary],
) -> Vec<SourceActivity> {
    let mut by_source: HashMap<String, SourceActivity> = HashMap::new();
    for workflow in workflows {
        add_workflow_source_activity(&mut by_source, workflow);
    }
    for task in active_tasks {
        if task.workflow.is_some() {
            continue;
        }
        add_legacy_source_activity(&mut by_source, task);
    }
    let mut rows: Vec<SourceActivity> = by_source.into_values().collect();
    rows.sort_by(|a, b| {
        source_total(b)
            .cmp(&source_total(a))
            .then_with(|| a.source.cmp(&b.source))
    });
    rows
}

fn add_workflow_source_activity(
    by_source: &mut HashMap<String, SourceActivity>,
    workflow: &WorkflowInstance,
) {
    let source = workflow_source(workflow);
    let entry = by_source
        .entry(source.clone())
        .or_insert_with(|| SourceActivity {
            source,
            ..Default::default()
        });
    match workflow_bucket(workflow) {
        WorkflowBucket::Running => entry.running += 1,
        WorkflowBucket::Blocked => entry.blocked += 1,
        WorkflowBucket::Failed => entry.failed += 1,
        WorkflowBucket::ReadyToMerge => entry.ready_to_merge += 1,
        _ => {}
    }
}

fn add_legacy_source_activity(by_source: &mut HashMap<String, SourceActivity>, task: &TaskSummary) {
    let update: fn(&mut SourceActivity) = if task.status.is_failure()
        || task.scheduler.authority_state == SchedulerAuthorityState::Failed
    {
        |entry| entry.failed += 1
    } else if matches!(
        task.scheduler.authority_state,
        SchedulerAuthorityState::Running
            | SchedulerAuthorityState::Leased
            | SchedulerAuthorityState::Recovering
    ) {
        |entry| entry.running += 1
    } else if matches!(
        task.scheduler.authority_state,
        SchedulerAuthorityState::AwaitingDependencies | SchedulerAuthorityState::RetryBackoff
    ) || matches!(task.status.as_ref(), "awaiting_deps" | "waiting")
    {
        |entry| entry.blocked += 1
    } else {
        return;
    };
    let source = task
        .source
        .as_deref()
        .filter(|source| !source.trim().is_empty())
        .unwrap_or("manual")
        .to_string();
    let entry = by_source
        .entry(source.clone())
        .or_insert_with(|| SourceActivity {
            source,
            ..Default::default()
        });
    update(entry);
}

fn source_total(source: &SourceActivity) -> u64 {
    source.running + source.blocked + source.failed + source.ready_to_merge
}

fn operator_actions(
    workflows: &[WorkflowInstance],
    generated_at: DateTime<Utc>,
) -> Vec<OperatorAction> {
    let mut actions = Vec::new();
    for workflow in workflows {
        let Some((kind, next_action)) = workflow_action_kind(workflow.state.as_str()) else {
            continue;
        };
        let projection = RuntimeWorkflowProjection::from_workflow(workflow);
        let task_id = projection
            .submission_handle
            .as_ref()
            .map(|task_id| task_id.as_str().to_string());
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

fn workflow_action_kind(state: &str) -> Option<(&'static str, &'static str)> {
    match state {
        "ready_to_merge" => Some(("ready_to_merge", "Review and merge")),
        "awaiting_feedback" => Some(("awaiting_feedback", "Inspect review feedback")),
        "blocked" => Some(("blocked", "Resolve blocker")),
        _ => None,
    }
}

fn action_priority(kind: &str) -> u8 {
    match kind {
        "ready_to_merge" => 0,
        "blocked" => 1,
        "awaiting_feedback" => 2,
        _ => 3,
    }
}

fn grouped_failures(failures: &[RecentFailureTask]) -> Vec<FailureGroup> {
    let mut groups: HashMap<FailureGroupKey, FailureGroup> = HashMap::new();
    for failure in failures {
        let raw_message = failure.error.as_deref().unwrap_or("unknown failure");
        let family = classify_failure_family(raw_message);
        let key = FailureGroupKey {
            family,
            message: normalize_failure_message(raw_message),
            repo: failure.project.as_deref().map(project_label),
        };
        let failed_at = failure.failed_at.clone();
        let task_id = failure.id.as_str().to_string();
        let group = groups.entry(key.clone()).or_insert_with(|| FailureGroup {
            family,
            severity: failure_severity(family),
            message: key.message.clone(),
            first_seen: failed_at.clone(),
            last_seen: failed_at.clone(),
            count: 0,
            repo: key.repo.clone(),
            task_id: Some(task_id.clone()),
            retryable: failure_retryable(family),
            next_action: failure_next_action(family),
        });
        group.count += 1;
        group.first_seen = earlier_timestamp(group.first_seen.as_deref(), failed_at.as_deref());
        group.last_seen = later_timestamp(group.last_seen.as_deref(), failed_at.as_deref());
        if group.task_id.is_none() {
            group.task_id = Some(task_id);
        }
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
    Some(
        match (current, candidate) {
            (Some(current), Some(candidate)) => current.min(candidate),
            (Some(current), None) => current,
            (None, Some(candidate)) => candidate,
            (None, None) => return None,
        }
        .to_string(),
    )
}

fn later_timestamp(current: Option<&str>, candidate: Option<&str>) -> Option<String> {
    Some(
        match (current, candidate) {
            (Some(current), Some(candidate)) => current.max(candidate),
            (Some(current), None) => current,
            (None, Some(candidate)) => candidate,
            (None, None) => return None,
        }
        .to_string(),
    )
}

fn stale_worktree_count(
    manager: Option<&WorkspaceManager>,
    cards: &[crate::handlers::worktrees::WorktreeResponse],
) -> Option<u64> {
    manager.map(|manager| manager.live_count().saturating_sub(cards.len() as u64))
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
            harness_workflow::runtime::REPO_BACKLOG_DEFINITION_ID => "repo_backlog".to_string(),
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
mod tests {
    use super::*;
    use crate::task_runner::TaskWorkflowSummary;
    use crate::test_helpers;
    use axum::{body::to_bytes, routing::get, Router};
    use harness_core::types::TaskId;
    use harness_workflow::runtime::{WorkflowSubject, GITHUB_ISSUE_PR_DEFINITION_ID};

    fn workflow(state: &str, data: Value) -> WorkflowInstance {
        WorkflowInstance::new(
            GITHUB_ISSUE_PR_DEFINITION_ID,
            1,
            state,
            WorkflowSubject::new("issue", "issue:1"),
        )
        .with_data(data)
    }

    fn github_fetch_failure(id: &str, issue: u64, failed_at: &str) -> RecentFailureTask {
        RecentFailureTask {
            id: TaskId(id.to_string()),
            failure_kind: None,
            external_id: Some(format!("issue:{issue}")),
            project: Some("/tmp/harness".to_string()),
            workspace_path: None,
            workspace_owner: None,
            run_generation: 0,
            error: Some(
                "git fetch origin main failed: LibreSSL SSL_connect: SSL_ERROR_SYSCALL in connection to github.com:443"
                    .to_string(),
            ),
            failed_at: Some(failed_at.to_string()),
        }
    }

    #[test]
    fn runtime_workflow_counts_reconcile_execution_review_and_terminal_states() {
        let workflows = vec![
            workflow("implementing", json!({})),
            workflow("awaiting_feedback", json!({})),
            workflow("ready_to_merge", json!({})),
            workflow("awaiting_dependencies", json!({})),
            workflow("failed", json!({})),
            workflow("done", json!({})),
        ];

        let counts = runtime_workflow_counts(&workflows);

        assert_eq!(counts.running, 1);
        assert_eq!(counts.review, 1);
        assert_eq!(counts.ready_to_merge, 1);
        assert_eq!(counts.awaiting_dependencies, 1);
        assert_eq!(counts.failed, 1);
        assert_eq!(counts.done, 1);
    }

    #[test]
    fn grouped_failures_classifies_and_counts_github_fetch_failures() {
        let failures = vec![
            github_fetch_failure("task-1", 1, "2026-06-12T00:00:00Z"),
            github_fetch_failure("task-2", 2, "2026-06-12T00:05:00Z"),
        ];

        let groups = grouped_failures(&failures);

        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].family, "github_fetch");
        assert_eq!(groups[0].severity, "warn");
        assert_eq!(groups[0].count, 2);
        assert_eq!(groups[0].repo.as_deref(), Some("harness"));
        assert!(groups[0].retryable);
        assert_eq!(groups[0].last_seen.as_deref(), Some("2026-06-12T00:05:00Z"));
    }

    #[tokio::test]
    async fn endpoint_returns_monitor_payload_on_fresh_state() -> anyhow::Result<()> {
        let _lock = test_helpers::HOME_LOCK.lock().await;
        let dir = test_helpers::tempdir_in_home("harness-test-operator-monitor-")?;
        let state = Arc::new(test_helpers::make_test_state(dir.path()).await?);

        let app = Router::new()
            .route("/api/operator-monitor", get(operator_monitor))
            .with_state(state);

        let req = axum::http::Request::builder()
            .uri("/api/operator-monitor")
            .body(axum::body::Body::empty())?;
        let resp = tower::ServiceExt::oneshot(app, req).await?;
        assert_eq!(resp.status(), StatusCode::OK);

        let bytes = to_bytes(resp.into_body(), usize::MAX).await?;
        let body: Value = serde_json::from_slice(&bytes)?;

        for key in [
            "generated_at",
            "health",
            "activity",
            "operator_actions",
            "failures",
            "worktrees",
        ] {
            assert!(body.get(key).is_some(), "missing top-level key: {key}");
        }
        assert_eq!(body["worktrees"]["metrics_state"], "unavailable");
        Ok(())
    }

    #[test]
    fn legacy_workflow_and_queued_tasks_are_not_counted_by_source() {
        let legacy_row = TaskSummary {
            id: TaskId("legacy-row".to_string()),
            task_kind: crate::task_runner::TaskKind::Issue,
            status: crate::task_runner::TaskStatus::Waiting,
            failure_kind: None,
            turn: 0,
            pr_url: None,
            error: None,
            source: Some("github".to_string()),
            parent_id: None,
            external_id: Some("issue:1".to_string()),
            repo: Some("owner/repo".to_string()),
            description: None,
            created_at: None,
            phase: crate::task_runner::TaskPhase::Review,
            depends_on: vec![],
            subtask_ids: vec![],
            project: None,
            workspace_path: None,
            workspace_owner: None,
            run_generation: 0,
            workflow: Some(TaskWorkflowSummary {
                id: "workflow-1".to_string(),
                definition_id: Some(GITHUB_ISSUE_PR_DEFINITION_ID.to_string()),
                state: "ready_to_merge".to_string(),
                project_id: None,
                issue_number: Some(1),
                pr_number: Some(7),
                force_execute: None,
                plan_concern: None,
                review_fallback: None,
            }),
            scheduler: crate::task_runner::TaskSchedulerState::queued(),
        };
        let mut queued_row = legacy_row.clone();
        queued_row.id = TaskId("queued-row".to_string());
        queued_row.workflow = None;
        queued_row.status = crate::task_runner::TaskStatus::Pending;

        let mut by_source = source_activity(
            &[workflow(
                "ready_to_merge",
                json!({
                    "source": "github",
                    "pr_number": 7,
                    "pr_url": "https://github.com/owner/repo/pull/7",
                }),
            )],
            &[legacy_row, queued_row],
        );

        assert_eq!(by_source.len(), 1);
        let source = by_source.pop().expect("source row");
        assert_eq!(source.source, "github");
        assert_eq!(source.ready_to_merge, 1);
        assert_eq!(source.running, 0);
        assert_eq!(source.blocked, 0);
    }
}
