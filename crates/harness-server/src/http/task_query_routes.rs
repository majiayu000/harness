use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::Serialize;
use serde_json::json;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use super::state::AppState;
use crate::task_runner::{
    SchedulerAuthorityState, TaskFailureKind, TaskKind, TaskPhase, TaskSchedulerState, TaskState,
    TaskStatus, TaskSummary, TaskWorkflowSummary,
};
use harness_core::proof_of_work::{CiStatus, ProofOfWork, QualitySignal, ReviewOutcome};

/// Response type for `GET /tasks/{id}` — `TaskState` fields plus the optional workflow summary
/// that requires a separate workflow-store lookup (not persisted on `TaskState` itself).
#[derive(Serialize)]
struct FullTaskResponse {
    #[serde(flatten)]
    inner: crate::task_runner::TaskState,
    #[serde(skip_serializing_if = "Option::is_none")]
    workflow: Option<TaskWorkflowSummary>,
}

#[derive(Serialize)]
struct RuntimeTaskResponse {
    id: String,
    task_id: String,
    task_kind: TaskKind,
    status: String,
    execution_path: &'static str,
    workflow_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    external_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    repo: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    project: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    issue: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    workflow: Option<TaskWorkflowSummary>,
}

pub(crate) async fn list_tasks(State(state): State<Arc<AppState>>) -> Response {
    match state.core.tasks.list_all_summaries_with_terminal().await {
        Ok(mut summaries) => {
            if let Some(workflow_store) = state.core.issue_workflow_store.as_ref() {
                // Bulk-fetch all workflows once to avoid O(N) sequential DB round trips.
                let workflows = workflow_store.list().await.unwrap_or_else(|e| {
                    tracing::error!("list_tasks: failed to bulk-fetch workflows: {e}");
                    Vec::new()
                });
                let mut workflows_by_issue = HashMap::new();
                let mut workflows_by_pr = HashMap::new();
                for wf in workflows {
                    let issue_key = (wf.project_id.clone(), wf.repo.clone(), wf.issue_number);
                    if let Some(pr_num) = wf.pr_number {
                        let pr_key = (wf.project_id.clone(), wf.repo.clone(), pr_num);
                        workflows_by_issue
                            .entry(issue_key)
                            .or_insert_with(|| wf.clone());
                        workflows_by_pr.entry(pr_key).or_insert(wf);
                    } else {
                        workflows_by_issue.entry(issue_key).or_insert(wf);
                    }
                }
                for summary in &mut summaries {
                    let Some(project_id) = summary.project.as_deref() else {
                        continue;
                    };
                    let by_issue = summary
                        .external_id
                        .as_deref()
                        .and_then(|id| id.strip_prefix("issue:"))
                        .and_then(|n| n.parse::<u64>().ok());
                    let by_pr = summary
                        .external_id
                        .as_deref()
                        .and_then(|id| id.strip_prefix("pr:"))
                        .and_then(|n| n.parse::<u64>().ok())
                        .or_else(|| {
                            summary
                                .pr_url
                                .as_deref()
                                .and_then(crate::http::parse_pr_num_from_url)
                        });
                    summary.workflow = match (by_issue, by_pr) {
                        (Some(issue), _) => workflows_by_issue
                            .get(&(project_id.to_owned(), summary.repo.clone(), issue))
                            .cloned()
                            .map(TaskWorkflowSummary::from),
                        (None, Some(pr)) => workflows_by_pr
                            .get(&(project_id.to_owned(), summary.repo.clone(), pr))
                            .cloned()
                            .map(TaskWorkflowSummary::from),
                        (None, None) => None,
                    };
                }
            }
            if let Err(e) = append_runtime_submission_summaries(&state, &mut summaries).await {
                tracing::error!("list_tasks: failed to append runtime submission summaries: {e}");
            }
            Json(summaries).into_response()
        }
        Err(e) => {
            tracing::error!("list_tasks: database error: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "internal server error"})),
            )
                .into_response()
        }
    }
}

async fn append_runtime_submission_summaries(
    state: &AppState,
    summaries: &mut Vec<TaskSummary>,
) -> anyhow::Result<()> {
    let Some(store) = state.core.workflow_runtime_store.as_ref() else {
        return Ok(());
    };
    append_runtime_definition_summaries(
        store,
        summaries,
        harness_workflow::runtime::GITHUB_ISSUE_PR_DEFINITION_ID,
        TaskKind::Issue,
    )
    .await?;
    append_runtime_definition_summaries(
        store,
        summaries,
        harness_workflow::runtime::PROMPT_TASK_DEFINITION_ID,
        TaskKind::Prompt,
    )
    .await
}

async fn append_runtime_definition_summaries(
    store: &harness_workflow::runtime::WorkflowRuntimeStore,
    summaries: &mut Vec<TaskSummary>,
    definition_id: &str,
    task_kind: TaskKind,
) -> anyhow::Result<()> {
    let workflows = store
        .list_instances_by_definition(definition_id, None, None)
        .await?;
    let mut listed_ids: HashSet<String> = summaries
        .iter()
        .map(|summary| summary.id.as_str().to_string())
        .collect();
    for workflow in workflows {
        let Some(task_id) =
            crate::workflow_runtime_submission::runtime_issue_task_handle(&workflow)
        else {
            continue;
        };
        if !listed_ids.insert(task_id.as_str().to_string()) {
            continue;
        }
        summaries.push(runtime_workflow_task_summary(workflow, task_id, task_kind));
    }
    Ok(())
}

fn runtime_workflow_task_summary(
    workflow: harness_workflow::runtime::WorkflowInstance,
    task_id: harness_core::types::TaskId,
    task_kind: TaskKind,
) -> TaskSummary {
    let status = runtime_workflow_state_to_task_status(&workflow.state);
    let scheduler = runtime_workflow_scheduler_state(&status);
    let issue = workflow
        .data
        .get("issue_number")
        .and_then(|value| value.as_u64());
    let external_id = match task_kind {
        TaskKind::Issue => issue.map(|issue_number| format!("issue:{issue_number}")),
        TaskKind::Prompt => runtime_string_field(&workflow.data, "external_id"),
        _ => None,
    };
    let description = match task_kind {
        TaskKind::Issue => Some(
            issue
                .map(|issue_number| format!("issue #{issue_number}"))
                .unwrap_or_else(|| workflow.subject.subject_key.clone()),
        ),
        TaskKind::Prompt => Some(
            runtime_string_field(&workflow.data, "prompt_summary")
                .unwrap_or_else(|| "prompt task".to_string()),
        ),
        _ => Some(workflow.subject.subject_key.clone()),
    };
    TaskSummary {
        id: task_id,
        task_kind,
        status: status.clone(),
        failure_kind: status.is_failure().then_some(TaskFailureKind::Task),
        turn: 0,
        pr_url: runtime_string_field(&workflow.data, "pr_url"),
        error: runtime_string_field(&workflow.data, "failure_reason"),
        source: runtime_string_field(&workflow.data, "source"),
        parent_id: None,
        external_id,
        repo: runtime_string_field(&workflow.data, "repo"),
        description,
        created_at: Some(workflow.created_at.to_rfc3339()),
        phase: runtime_workflow_state_to_task_phase(&workflow.state),
        depends_on: runtime_task_id_array(&workflow.data, "depends_on"),
        subtask_ids: Vec::new(),
        project: runtime_string_field(&workflow.data, "project_id"),
        workspace_path: None,
        workspace_owner: None,
        run_generation: 0,
        workflow: Some(TaskWorkflowSummary::from_runtime_workflow(&workflow)),
        scheduler,
    }
}

fn runtime_workflow_state_to_task_status(state: &str) -> TaskStatus {
    match state {
        "awaiting_dependencies" => TaskStatus::AwaitingDeps,
        "scheduled" | "discovered" => TaskStatus::Pending,
        "implementing" | "replanning" | "addressing_feedback" => TaskStatus::Implementing,
        "pr_open" | "awaiting_feedback" | "ready_to_merge" | "blocked" => TaskStatus::Waiting,
        "done" | "passed" => TaskStatus::Done,
        "failed" => TaskStatus::Failed,
        "cancelled" => TaskStatus::Cancelled,
        _ => TaskStatus::Waiting,
    }
}

fn runtime_workflow_state_to_task_phase(state: &str) -> TaskPhase {
    match state {
        "done" | "passed" | "failed" | "cancelled" => TaskPhase::Terminal,
        "pr_open" | "awaiting_feedback" | "ready_to_merge" => TaskPhase::Review,
        "replanning" | "blocked" => TaskPhase::Plan,
        _ => TaskPhase::Implement,
    }
}

fn runtime_workflow_scheduler_state(status: &TaskStatus) -> TaskSchedulerState {
    match status {
        TaskStatus::AwaitingDeps => TaskSchedulerState::awaiting_dependencies(),
        TaskStatus::Pending => TaskSchedulerState::queued(),
        TaskStatus::Done | TaskStatus::Failed | TaskStatus::Cancelled => {
            let mut scheduler = TaskSchedulerState::queued();
            scheduler.mark_terminal(status);
            scheduler
        }
        _ => TaskSchedulerState {
            authority_state: SchedulerAuthorityState::Running,
            owner: None,
            run_generation: 0,
            recovery_generation: 0,
            lease_expires_at: None,
        },
    }
}

fn runtime_string_field(data: &serde_json::Value, field: &str) -> Option<String> {
    data.get(field)
        .and_then(|value| value.as_str())
        .map(ToOwned::to_owned)
}

fn runtime_task_id_array(
    data: &serde_json::Value,
    field: &str,
) -> Vec<harness_core::types::TaskId> {
    data.get(field)
        .and_then(|value| value.as_array())
        .into_iter()
        .flatten()
        .filter_map(|value| value.as_str())
        .map(harness_core::types::TaskId::from_str)
        .collect()
}

pub(crate) async fn get_task(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Response {
    let task_id = harness_core::types::TaskId(id);
    match state.core.tasks.get_with_db_fallback(&task_id).await {
        Ok(Some(task)) => {
            let workflow = enrich_task_workflow(&state, &task).await;
            Json(FullTaskResponse {
                inner: task,
                workflow,
            })
            .into_response()
        }
        Ok(None) => match runtime_task_response_by_handle(&state, &task_id).await {
            Ok(Some(runtime_task)) => Json(runtime_task).into_response(),
            Ok(None) => (
                StatusCode::NOT_FOUND,
                Json(json!({"error": "task not found"})),
            )
                .into_response(),
            Err(e) => {
                tracing::error!("get_task: runtime workflow lookup failed: {e}");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": "internal server error"})),
                )
                    .into_response()
            }
        },
        Err(e) => {
            tracing::error!("get_task: database error: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "internal server error"})),
            )
                .into_response()
        }
    }
}

async fn runtime_task_response_by_handle(
    state: &AppState,
    task_id: &harness_core::types::TaskId,
) -> anyhow::Result<Option<RuntimeTaskResponse>> {
    let Some(store) = state.core.workflow_runtime_store.as_ref() else {
        return Ok(None);
    };
    let Some(workflow) = store.get_instance_by_task_id(task_id.as_str()).await? else {
        return Ok(None);
    };
    let issue = workflow
        .data
        .get("issue_number")
        .and_then(|value| value.as_u64());
    let Some(task_kind) = (match workflow.definition_id.as_str() {
        harness_workflow::runtime::GITHUB_ISSUE_PR_DEFINITION_ID => Some(TaskKind::Issue),
        harness_workflow::runtime::PROMPT_TASK_DEFINITION_ID => Some(TaskKind::Prompt),
        _ => None,
    }) else {
        return Ok(None);
    };
    let external_id = match task_kind {
        TaskKind::Issue => issue.map(|issue_number| format!("issue:{issue_number}")),
        TaskKind::Prompt => runtime_string_field(&workflow.data, "external_id"),
        _ => None,
    };
    Ok(Some(RuntimeTaskResponse {
        id: task_id.as_str().to_string(),
        task_id: task_id.as_str().to_string(),
        task_kind,
        status: workflow.state.clone(),
        execution_path: "workflow_runtime",
        workflow_id: workflow.id.clone(),
        external_id,
        repo: workflow
            .data
            .get("repo")
            .and_then(|value| value.as_str())
            .map(ToOwned::to_owned),
        project: workflow
            .data
            .get("project_id")
            .and_then(|value| value.as_str())
            .map(ToOwned::to_owned),
        issue,
        workflow: Some(TaskWorkflowSummary::from_runtime(&workflow)),
    }))
}

/// Look up the issue-workflow instance for a task using targeted store queries.
/// Returns `None` when the workflow store is unavailable or the task has no workflow association.
async fn enrich_task_workflow(
    state: &AppState,
    task: &crate::task_runner::TaskState,
) -> Option<TaskWorkflowSummary> {
    let workflow_store = state.core.issue_workflow_store.as_ref()?;
    let project_id = task.project_root.as_ref()?.to_string_lossy().into_owned();

    let by_issue = task
        .external_id
        .as_deref()
        .and_then(|id| id.strip_prefix("issue:"))
        .and_then(|n| n.parse::<u64>().ok());
    let by_pr = task
        .external_id
        .as_deref()
        .and_then(|id| id.strip_prefix("pr:"))
        .and_then(|n| n.parse::<u64>().ok())
        .or_else(|| {
            task.pr_url
                .as_deref()
                .and_then(super::parse_pr_num_from_url)
        });

    match (by_issue, by_pr) {
        (Some(issue), _) => workflow_store
            .get_by_issue(&project_id, task.repo.as_deref(), issue)
            .await
            .unwrap_or_else(|e| {
                tracing::warn!("get_task: workflow lookup by issue failed: {e}");
                None
            })
            .map(TaskWorkflowSummary::from),
        (None, Some(pr)) => workflow_store
            .get_by_pr(&project_id, task.repo.as_deref(), pr)
            .await
            .unwrap_or_else(|e| {
                tracing::warn!("get_task: workflow lookup by PR failed: {e}");
                None
            })
            .map(TaskWorkflowSummary::from),
        (None, None) => None,
    }
}

/// GET /tasks/{id}/artifacts — all persisted artifacts for a task.
pub(crate) async fn get_task_artifacts(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Response {
    let task_id = harness_core::types::TaskId(id);
    match state.core.tasks.get_with_db_fallback(&task_id).await {
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"error": "task not found"})),
            )
                .into_response();
        }
        Err(e) => {
            tracing::error!("get_task_artifacts: database error: {e}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "internal server error"})),
            )
                .into_response();
        }
        Ok(Some(_)) => {}
    }
    match state.core.tasks.list_artifacts(&task_id).await {
        Ok(artifacts) => Json(artifacts).into_response(),
        Err(e) => {
            tracing::error!("get_task_artifacts: list artifacts error: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "internal server error"})),
            )
                .into_response()
        }
    }
}

/// Derive a [`ProofOfWork`] summary from a [`TaskState`].
///
/// Uses only `TaskState` fields already populated by the review loop, so the
/// derivation is a pure function and stays cheap to call from HTTP handlers.
///
/// CI status policy:
/// - `Passed` requires both terminal `Done` status and an approving review,
///   because the LGTM gate is what runs the project's test command.
/// - `Failed` covers explicit `Failed` status and review rounds that recorded
///   a `quota_exhausted` / `billing_failed` / `upstream_failure` / `timeout`
///   result.
/// - Everything else (cancelled, no review evidence) reports `Unknown`.
pub(crate) fn proof_from_state(task: &TaskState) -> ProofOfWork {
    let mut review_rounds: u32 = 0;
    let mut last_review_result: Option<&str> = None;
    let mut saw_review_failure = false;

    for round in &task.rounds {
        if !matches!(round.action.as_str(), "review" | "agent_review") {
            continue;
        }
        review_rounds = review_rounds.saturating_add(1);
        last_review_result = Some(round.result.as_str());
        if matches!(
            round.result.as_str(),
            "quota_exhausted" | "billing_failed" | "upstream_failure" | "timeout" | "failed"
        ) {
            saw_review_failure = true;
        }
    }

    let review_outcome = match last_review_result {
        Some("lgtm") | Some("approved") => ReviewOutcome::Approved,
        Some("needs_fix") | Some("fixed") => ReviewOutcome::ChangesRequested,
        Some(result) if result.ends_with(" issues") => ReviewOutcome::ChangesRequested,
        Some(_) => ReviewOutcome::Skipped,
        None => ReviewOutcome::Skipped,
    };

    let ci_status =
        if matches!(task.status, TaskStatus::Done) && review_outcome == ReviewOutcome::Approved {
            CiStatus::Passed
        } else if matches!(task.status, TaskStatus::Failed) || saw_review_failure {
            CiStatus::Failed
        } else {
            CiStatus::Unknown
        };

    let mut signals = Vec::new();
    signals.push(QualitySignal {
        name: "turns".to_string(),
        value: task.turn.to_string(),
    });
    if let Some(error) = task.error.as_deref().filter(|s| !s.is_empty()) {
        signals.push(QualitySignal {
            name: "error".to_string(),
            value: error.to_string(),
        });
    }

    ProofOfWork {
        task_id: task.id.0.clone(),
        status: task.status.as_ref().to_string(),
        pr_url: task.pr_url.clone(),
        ci_status,
        review_outcome,
        review_rounds,
        quality_signals: signals,
    }
}

/// GET /tasks/{id}/proof — machine-readable proof-of-work summary for a
/// completed task.
///
/// Returns 404 for unknown task IDs, 422 while the task is still in flight,
/// and 200 with a JSON [`ProofOfWork`] for terminal tasks (Done / Failed /
/// Cancelled). See `harness-core::proof_of_work` for the wire format.
pub(crate) async fn get_task_proof(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Response {
    let task_id = harness_core::types::TaskId(id);
    match state.core.tasks.get_with_db_fallback(&task_id).await {
        Ok(Some(task)) => {
            if !task.status.is_terminal() {
                return (
                    StatusCode::UNPROCESSABLE_ENTITY,
                    Json(json!({
                        "error": "task is not in a terminal state",
                        "status": task.status.as_ref(),
                    })),
                )
                    .into_response();
            }
            Json(proof_from_state(&task)).into_response()
        }
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "task not found"})),
        )
            .into_response(),
        Err(e) => {
            tracing::error!("get_task_proof: database error: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "internal server error"})),
            )
                .into_response()
        }
    }
}

/// GET /tasks/{id}/prompts — all persisted redacted prompts for a task.
pub(crate) async fn get_task_prompts(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Response {
    let task_id = harness_core::types::TaskId(id);
    match state.core.tasks.get_with_db_fallback(&task_id).await {
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"error": "task not found"})),
            )
                .into_response();
        }
        Err(e) => {
            tracing::error!("get_task_prompts: database error: {e}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "internal server error"})),
            )
                .into_response();
        }
        Ok(Some(_)) => {}
    }
    match state.core.tasks.get_prompts(&task_id).await {
        Ok(prompts) => Json(prompts).into_response(),
        Err(e) => {
            tracing::error!("get_task_prompts: query error: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "internal server error"})),
            )
                .into_response()
        }
    }
}

#[cfg(test)]
#[path = "task_query_routes_tests.rs"]
mod tests;
