//! GET /api/operator-snapshot — live operator diagnostic view.
//!
//! Aggregates retry scheduler state, rate-limit pressure, and recent task
//! failures into a single low-latency payload suitable for polling every 30 s.
//!
//! `retry.stalled_tasks` and `recent_failures` merge TaskStore rows with
//! workflow-runtime instances so runtime-only work is visible to operators.

use crate::http::rest_contract::ContractJson;
use crate::http::AppState;
use crate::runtime_projection::RuntimeWorkflowProjection;
use crate::task_runner::TaskStatus;
use axum::{extract::State, http::StatusCode};
use chrono::{DateTime, Utc};
use harness_core::types::EventFilters;
use harness_protocol::rest::OperatorSnapshotResponse;
use harness_workflow::runtime::{
    WorkflowDefinitionRegistry, WorkflowInstance, WorkflowRuntimeStore, WorkflowTerminalState,
    QUALITY_GATE_DEFINITION_ID,
};
use serde_json::{json, Value};
use std::cmp::Reverse;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

/// Tasks stalled for longer than this are included in the snapshot.
/// Approximation for operator visibility only — does not affect retry decisions.
const SNAPSHOT_STALE_MINS: u64 = 30;

/// Maximum stalled / recent-failure tasks returned to keep the payload bounded.
const MAX_TASKS: usize = 20;

/// Maximum length of a task error string before truncation.
const MAX_ERROR_LEN: usize = 200;

type SnapshotJson = ContractJson<OperatorSnapshotResponse>;

fn snapshot_json(value: Value) -> SnapshotJson {
    ContractJson(OperatorSnapshotResponse(value))
}

fn stalled_task_json(t: &crate::task_runner::TaskState) -> Value {
    json!({
        "task_id":      t.id.0,
        "external_id":  t.external_id.as_deref().unwrap_or("—"),
        "project":      t.project_root.as_ref()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "—".to_string()),
        "workspace_path": t.workspace_path.as_ref()
            .map(|p| p.to_string_lossy().into_owned()),
        "workspace_owner": t.workspace_owner.clone(),
        "run_generation": t.run_generation,
        "status":       t.status.as_ref(),
        "stalled_since": t.updated_at.as_deref(),
    })
}

fn recent_failure_json(t: &crate::task_runner::RecentFailureTask) -> Value {
    let terminal = crate::task_runner::TaskTerminalInfo::from_status_error(
        &crate::task_runner::TaskStatus::Failed,
        t.error.as_deref(),
    );
    let error = t
        .error
        .as_deref()
        .map(|e| {
            if e.len() > MAX_ERROR_LEN {
                // Walk back to a valid char boundary to avoid panicking
                // on multi-byte UTF-8 characters at the cut point.
                let mut boundary = MAX_ERROR_LEN;
                while boundary > 0 && !e.is_char_boundary(boundary) {
                    boundary -= 1;
                }
                format!("{}…", &e[..boundary])
            } else {
                e.to_string()
            }
        })
        .unwrap_or_else(|| "—".to_string());

    json!({
        "task_id":    t.id.0,
        "failure_kind": t.failure_kind.as_ref().map(|kind| kind.as_ref()),
        "external_id": t.external_id.as_deref().unwrap_or("—"),
        "project":    t.project.as_deref()
            .and_then(|p| std::path::Path::new(p).file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "—".to_string()),
        "workspace_path": t.workspace_path.as_deref(),
        "workspace_owner": t.workspace_owner.as_deref(),
        "run_generation": t.run_generation,
        "error":      error,
        "terminal": terminal,
        "failed_at":  t.failed_at.as_deref(),
    })
}

fn workflow_display_task_id(workflow: &WorkflowInstance) -> String {
    crate::runtime_projection::runtime_submission_handle(&workflow.data)
        .or_else(|| crate::runtime_projection::legacy_dedupe_task_handle(&workflow.data))
        .map(|task_id| task_id.0)
        .unwrap_or_else(|| workflow.id.clone())
}

fn workflow_identity_ids(workflow: &WorkflowInstance) -> HashSet<String> {
    let mut ids = HashSet::new();
    ids.insert(workflow.id.clone());
    if let Some(task_id) = crate::runtime_projection::runtime_submission_handle(&workflow.data) {
        ids.insert(task_id.as_str().to_string());
    }
    if let Some(task_id) = crate::runtime_projection::legacy_dedupe_task_handle(&workflow.data) {
        ids.insert(task_id.0);
    }
    ids
}

fn workflow_failure_message(workflow: &WorkflowInstance) -> String {
    ["failure_reason", "previous_error", "last_error", "error"]
        .into_iter()
        .find_map(|field| {
            workflow
                .data
                .get(field)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(ToOwned::to_owned)
        })
        .unwrap_or_else(|| format!("{} workflow failed", workflow.definition_id))
}

fn project_basename(path: Option<&str>) -> String {
    path.and_then(|p| std::path::Path::new(p).file_name())
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "—".to_string())
}

fn stalled_workflow_json(
    registry: &WorkflowDefinitionRegistry,
    workflow: &WorkflowInstance,
) -> Value {
    let projection = RuntimeWorkflowProjection::from_workflow_with_registry(registry, workflow);
    json!({
        "task_id": workflow_display_task_id(workflow),
        "external_id": workflow.subject.subject_key.as_str(),
        "project": project_basename(projection.project_id.as_deref()),
        "workspace_path": Value::Null,
        "workspace_owner": Value::Null,
        "run_generation": 0,
        "status": projection.task_status.as_ref(),
        "stalled_since": workflow.updated_at.to_rfc3339(),
    })
}

fn recent_failure_from_workflow(
    registry: &WorkflowDefinitionRegistry,
    workflow: &WorkflowInstance,
) -> crate::task_runner::RecentFailureTask {
    let projection = RuntimeWorkflowProjection::from_workflow_with_registry(registry, workflow);
    crate::task_runner::RecentFailureTask {
        id: harness_core::types::TaskId(workflow_display_task_id(workflow)),
        failure_kind: projection.failure_kind,
        external_id: Some(workflow.subject.subject_key.clone()),
        project: projection.project_id,
        workspace_path: None,
        workspace_owner: None,
        run_generation: 0,
        error: Some(workflow_failure_message(workflow)),
        failed_at: Some(workflow.updated_at.to_rfc3339()),
    }
}

fn parse_rfc3339(value: Option<&str>) -> Option<DateTime<Utc>> {
    value
        .and_then(|raw| DateTime::parse_from_rfc3339(raw).ok())
        .map(|parsed| parsed.with_timezone(&Utc))
}

fn is_stalled_runtime_candidate(
    registry: &WorkflowDefinitionRegistry,
    workflow: &WorkflowInstance,
) -> bool {
    // Match TaskStore stalled semantics: queued / dependency-blocked work is
    // backlog, not a stuck execution.
    let status =
        RuntimeWorkflowProjection::from_workflow_with_registry(registry, workflow).task_status;
    !matches!(status, TaskStatus::Pending | TaskStatus::AwaitingDeps)
}

fn merge_stalled_json(
    tasks: &[crate::task_runner::TaskState],
    workflows: &[WorkflowInstance],
    registry: &WorkflowDefinitionRegistry,
) -> Vec<Value> {
    let mut seen = HashSet::new();
    let mut rows: Vec<(Option<DateTime<Utc>>, Value)> = Vec::new();

    for task in tasks {
        seen.insert(task.id.0.clone());
        rows.push((
            parse_rfc3339(task.updated_at.as_deref()),
            stalled_task_json(task),
        ));
    }
    for workflow in workflows {
        let identities = workflow_identity_ids(workflow);
        if identities.iter().any(|id| seen.contains(id)) {
            continue;
        }
        seen.extend(identities);
        rows.push((
            Some(workflow.updated_at),
            stalled_workflow_json(registry, workflow),
        ));
    }

    // Known timestamps first (oldest first); missing timestamps sort last.
    rows.sort_by_key(|(updated_at, _)| (updated_at.is_none(), *updated_at));
    rows.truncate(MAX_TASKS);
    rows.into_iter().map(|(_, row)| row).collect()
}

fn merge_recent_failure_json(
    tasks: &[crate::task_runner::RecentFailureTask],
    workflows: &[WorkflowInstance],
    registry: &WorkflowDefinitionRegistry,
) -> Vec<Value> {
    let mut seen = HashSet::new();
    let mut rows: Vec<(Option<DateTime<Utc>>, Value)> = Vec::new();

    for task in tasks {
        seen.insert(task.id.0.clone());
        rows.push((
            parse_rfc3339(task.failed_at.as_deref()),
            recent_failure_json(task),
        ));
    }
    for workflow in workflows {
        let identities = workflow_identity_ids(workflow);
        if identities.iter().any(|id| seen.contains(id)) {
            continue;
        }
        seen.extend(identities);
        let failure = recent_failure_from_workflow(registry, workflow);
        rows.push((
            parse_rfc3339(failure.failed_at.as_deref()),
            recent_failure_json(&failure),
        ));
    }

    rows.sort_by_key(|(failed_at, _)| Reverse(*failed_at));
    rows.truncate(MAX_TASKS);
    rows.into_iter().map(|(_, row)| row).collect()
}

async fn list_stalled_runtime_workflows(
    store: &WorkflowRuntimeStore,
    stale: Duration,
) -> anyhow::Result<Vec<WorkflowInstance>> {
    let cutoff = Utc::now() - chrono::Duration::from_std(stale)?;
    let definition_ids =
        crate::handlers::definition_ids::operator_definition_ids(store.definition_registry())?;
    let futures = definition_ids
        .iter()
        .map(|id| store.list_aged_root_nonterminal_instances_by_definition(id, cutoff, None));
    let results = futures::future::try_join_all(futures).await?;
    let registry = store.definition_registry();
    let mut workflows = results
        .into_iter()
        .flatten()
        .filter(|workflow| is_stalled_runtime_candidate(registry, workflow))
        .collect::<Vec<_>>();
    workflows.sort_by_key(|workflow| workflow.updated_at);
    workflows.truncate(MAX_TASKS);
    Ok(workflows)
}

async fn list_recent_failed_runtime_workflows(
    store: &WorkflowRuntimeStore,
) -> anyhow::Result<Vec<WorkflowInstance>> {
    let definition_ids =
        crate::handlers::definition_ids::operator_definition_ids(store.definition_registry())?;
    let futures = definition_ids.iter().map(|id| async move {
        // Quality-gate child failures propagate to the parent, so roots-only
        // avoids duplicate rows and child crowding. Other child definitions
        // (prompt_task, nested github_issue_pr, pr_feedback) do not propagate
        // terminal failure — keep those child rows visible.
        if id == QUALITY_GATE_DEFINITION_ID {
            store
                .list_recent_root_terminal_instances_by_definition(
                    id,
                    WorkflowTerminalState::Failed,
                    MAX_TASKS as i64,
                )
                .await
        } else {
            store
                .list_recent_terminal_instances_by_definition(
                    id,
                    WorkflowTerminalState::Failed,
                    MAX_TASKS as i64,
                )
                .await
        }
    });
    let results = futures::future::try_join_all(futures).await?;
    let mut workflows = results.into_iter().flatten().collect::<Vec<_>>();
    workflows.sort_by_key(|workflow| Reverse(workflow.updated_at));
    workflows.truncate(MAX_TASKS);
    Ok(workflows)
}

pub async fn operator_snapshot(State(state): State<Arc<AppState>>) -> (StatusCode, SnapshotJson) {
    let generated_at = Utc::now();
    let stale = Duration::from_secs(SNAPSHOT_STALE_MINS * 60);

    let recent_retry_filter = EventFilters {
        hook: Some("periodic_retry:summary".to_string()),
        since: Some(generated_at - chrono::Duration::hours(2)),
        ..Default::default()
    };
    let all_retry_filter = EventFilters {
        hook: Some("periodic_retry:summary".to_string()),
        ..Default::default()
    };
    // Collect all subsections concurrently — they are independent.
    let (retry_events_res, stalled_res, failed_res) = tokio::join!(
        state.observability.events.query(&recent_retry_filter),
        state.core.tasks.list_stalled_tasks(stale, None),
        state.core.tasks.list_recent_failed(MAX_TASKS as i64),
    );

    let mut retry_events = match retry_events_res {
        Ok(events) => events,
        Err(e) => return error_response(format!("failed to query retry events: {e}")),
    };
    let stalled_tasks = match stalled_res {
        Ok(tasks) => tasks,
        Err(e) => return error_response(format!("failed to query stalled tasks: {e}")),
    };
    let failed_tasks = match failed_res {
        Ok(tasks) => tasks,
        Err(e) => return error_response(format!("failed to query recent failures: {e}")),
    };
    if retry_events.is_empty() {
        retry_events = match state.observability.events.query(&all_retry_filter).await {
            Ok(events) => events,
            Err(e) => return error_response(format!("failed to query retry events: {e}")),
        };
    }

    let fallback_registry = WorkflowDefinitionRegistry::with_builtins();
    let (stalled_json, failures_json) = if let Some(store) =
        state.core.workflow_runtime_store.as_ref()
    {
        let (runtime_stalled_res, runtime_failed_res) = tokio::join!(
            list_stalled_runtime_workflows(store, stale),
            list_recent_failed_runtime_workflows(store),
        );
        let runtime_stalled = match runtime_stalled_res {
            Ok(workflows) => workflows,
            Err(e) => {
                return error_response(format!("failed to query stalled runtime workflows: {e}"))
            }
        };
        let runtime_failed = match runtime_failed_res {
            Ok(workflows) => workflows,
            Err(e) => {
                return error_response(format!("failed to query failed runtime workflows: {e}"))
            }
        };
        let registry = store.definition_registry();
        (
            merge_stalled_json(&stalled_tasks, &runtime_stalled, registry),
            merge_recent_failure_json(&failed_tasks, &runtime_failed, registry),
        )
    } else {
        (
            merge_stalled_json(&stalled_tasks, &[], &fallback_registry),
            merge_recent_failure_json(&failed_tasks, &[], &fallback_registry),
        )
    };

    // ---- retry section ----
    // query returns ASC order; the last element is the most recent tick.
    let last_tick: Value = retry_events
        .last()
        .and_then(|ev| {
            let detail = ev.detail.as_deref()?;
            let parsed: serde_json::Value = serde_json::from_str(detail).ok()?;
            Some(json!({
                "checked": parsed["checked"].as_u64().unwrap_or(0),
                "retried": parsed["retried"].as_u64().unwrap_or(0),
                "stuck":   parsed["stuck"].as_u64().unwrap_or(0),
                "skipped": parsed["skipped"].as_u64().unwrap_or(0),
                "at":      ev.ts.to_rfc3339(),
            }))
        })
        .unwrap_or(Value::Null);

    // ---- rate-limit section ----
    let sig_snap = state.observability.signal_rate_limiter.snapshot();
    let pw_snap = state.observability.password_reset_rate_limiter.snapshot();

    let runtime_logs = &state.core.server.runtime_logs;

    let body = json!({
        "generated_at": generated_at.to_rfc3339(),
        "retry": {
            "last_tick":     last_tick,
            "stalled_tasks": stalled_json,
        },
        "rate_limits": {
            "signal_ingestion": {
                "tracked_sources": sig_snap.tracked_sources,
                "limit_per_minute": sig_snap.limit_per_minute,
            },
            "password_reset": {
                "tracked_identifiers": pw_snap.tracked_identifiers,
                "limit_per_hour": pw_snap.limit_per_hour,
            },
        },
        "recent_failures": failures_json,
        "runtime_logs": {
            "state": runtime_logs.state.as_str(),
            "active_path": runtime_logs
                .active_path
                .as_ref()
                .map(|path| path.to_string_lossy().into_owned()),
            "path_hint": runtime_logs.path_hint.clone(),
            "retention_days": runtime_logs.retention_days,
            "retention_max_files": runtime_logs.retention_max_files,
        },
    });

    (StatusCode::OK, snapshot_json(body))
}

fn error_response(message: String) -> (StatusCode, SnapshotJson) {
    tracing::error!("operator_snapshot: {message}");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        snapshot_json(json!({ "error": message })),
    )
}

#[cfg(test)]
mod tests {
    include!("snapshot_cases.rs");
    include!("snapshot_runtime_cases.rs");
}
