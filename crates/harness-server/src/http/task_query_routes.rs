use axum::{
    extract::{rejection::QueryRejection, Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use super::state::AppState;
use crate::task_runner::{
    SchedulerAuthorityState, TaskFailureKind, TaskKind, TaskPhase, TaskSchedulerState, TaskState,
    TaskStatus, TaskSummary, TaskSummaryFilter, TaskSummaryPageCursor, TaskWorkflowSummary,
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

enum RuntimeProofLookup {
    Missing,
    InFlight(String),
    Terminal(ProofOfWork),
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawTaskListParams {
    status: Option<String>,
    scheduler_state: Option<String>,
    active: Option<bool>,
    kind: Option<String>,
    source: Option<String>,
    repo: Option<String>,
    project_id: Option<String>,
    limit: Option<u32>,
    cursor: Option<String>,
}

#[derive(Debug, Clone)]
struct TaskListQuery {
    filter: TaskSummaryFilter,
    limit: usize,
    cursor: Option<TaskListCursor>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TaskListCursor {
    created_at: chrono::DateTime<chrono::Utc>,
    id: String,
}

#[derive(Serialize)]
struct TaskListResponse {
    data: Vec<TaskSummary>,
    page: TaskListPage,
    counts: TaskListCounts,
}

#[derive(Serialize)]
struct TaskListPage {
    limit: usize,
    has_more: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    next_cursor: Option<String>,
}

#[derive(Serialize)]
struct TaskListCounts {
    total: usize,
    running: usize,
    failed: usize,
    by_status: BTreeMap<String, usize>,
    by_scheduler_state: BTreeMap<String, usize>,
}

const DEFAULT_TASK_LIST_LIMIT: u32 = 50;
const MAX_TASK_LIST_LIMIT: u32 = 500;

#[derive(Debug)]
struct TaskListQueryError {
    error: &'static str,
    message: String,
    hint: Option<&'static str>,
}

impl IntoResponse for TaskListQueryError {
    fn into_response(self) -> Response {
        (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": self.error,
                "message": self.message,
                "hint": self.hint,
                "allowed": {
                    "status": allowed_task_statuses(),
                    "scheduler_state": allowed_scheduler_states(),
                    "kind": ["issue", "pr", "prompt", "review", "planner"],
                }
            })),
        )
            .into_response()
    }
}

pub(crate) async fn list_tasks(
    State(state): State<Arc<AppState>>,
    query: Result<Query<RawTaskListParams>, QueryRejection>,
) -> Response {
    let raw_query = match query {
        Ok(Query(query)) => query,
        Err(error) => {
            return task_list_bad_request(
                "invalid_query",
                format!("Invalid /tasks query parameters: {error}"),
                None,
            )
            .into_response();
        }
    };
    let query = match TaskListQuery::from_raw(raw_query) {
        Ok(query) => query,
        Err(error) => return error.into_response(),
    };

    let task_store_cursor = query
        .cursor
        .as_ref()
        .map(TaskListCursor::as_task_summary_cursor);

    match state
        .core
        .tasks
        .list_summaries_filtered_page_with_terminal(
            &query.filter,
            task_store_cursor.as_ref(),
            query.limit + 1,
        )
        .await
    {
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
            if let Err(e) = append_runtime_submission_summaries(
                &state,
                &mut summaries,
                &query.filter,
                task_store_cursor.as_ref(),
                query.limit + 1,
            )
            .await
            {
                tracing::error!("list_tasks: failed to append runtime submission summaries: {e}");
            }
            sort_task_summaries(&mut summaries);
            let (data, page) = paginate_task_summaries(&summaries, &query);
            let counts = task_list_counts(&data);
            Json(TaskListResponse { data, page, counts }).into_response()
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

impl TaskListQuery {
    fn from_raw(raw: RawTaskListParams) -> Result<Self, TaskListQueryError> {
        let statuses = parse_task_statuses(raw.status.as_deref())?;
        let scheduler_states = parse_scheduler_states(raw.scheduler_state.as_deref())?;
        let active = raw.active.unwrap_or(false);
        if active && statuses.iter().any(TaskStatus::is_terminal) {
            return Err(task_list_bad_request(
                "invalid_filter",
                "`active=true` cannot be combined with terminal task statuses.",
                Some("Remove active=true or query terminal statuses without the active filter."),
            ));
        }
        let limit = raw.limit.unwrap_or(DEFAULT_TASK_LIST_LIMIT);
        if !(1..=MAX_TASK_LIST_LIMIT).contains(&limit) {
            return Err(task_list_bad_request(
                "invalid_limit",
                format!("limit must be between 1 and {MAX_TASK_LIST_LIMIT}."),
                None,
            ));
        }
        Ok(Self {
            filter: TaskSummaryFilter {
                statuses,
                scheduler_states,
                kinds: parse_task_kinds(raw.kind.as_deref())?,
                source: normalize_optional_query_value(raw.source),
                repo: normalize_optional_query_value(raw.repo),
                project: normalize_optional_query_value(raw.project_id),
                active,
            },
            limit: limit as usize,
            cursor: raw
                .cursor
                .as_deref()
                .map(parse_task_list_cursor)
                .transpose()?,
        })
    }
}

fn parse_task_statuses(raw: Option<&str>) -> Result<Vec<TaskStatus>, TaskListQueryError> {
    parse_csv(raw)
        .into_iter()
        .map(|value| {
            value.parse::<TaskStatus>().map_err(|_| {
                let hint = if value == "running" {
                    Some("Use scheduler_state=running instead of status=running.")
                } else {
                    None
                };
                task_list_bad_request(
                    "invalid_status",
                    format!("Unknown task status `{value}`."),
                    hint,
                )
            })
        })
        .collect()
}

fn parse_scheduler_states(
    raw: Option<&str>,
) -> Result<Vec<SchedulerAuthorityState>, TaskListQueryError> {
    parse_csv(raw)
        .into_iter()
        .map(|value| {
            value.parse::<SchedulerAuthorityState>().map_err(|_| {
                task_list_bad_request(
                    "invalid_scheduler_state",
                    format!("Unknown scheduler_state `{value}`."),
                    None,
                )
            })
        })
        .collect()
}

fn parse_task_kinds(raw: Option<&str>) -> Result<Vec<TaskKind>, TaskListQueryError> {
    parse_csv(raw)
        .into_iter()
        .map(|value| match value.as_str() {
            "issue" => Ok(TaskKind::Issue),
            "pr" => Ok(TaskKind::Pr),
            "prompt" => Ok(TaskKind::Prompt),
            "review" => Ok(TaskKind::Review),
            "planner" => Ok(TaskKind::Planner),
            _ => Err(task_list_bad_request(
                "invalid_kind",
                format!("Unknown task kind `{value}`."),
                None,
            )),
        })
        .collect()
}

fn parse_csv(raw: Option<&str>) -> Vec<String> {
    raw.unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

fn normalize_optional_query_value(value: Option<String>) -> Option<String> {
    value.and_then(|value| {
        let trimmed = value.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_string())
    })
}

fn parse_task_list_cursor(raw: &str) -> Result<TaskListCursor, TaskListQueryError> {
    let Some((created_at, id)) = raw.split_once('|') else {
        return Err(task_list_bad_request(
            "invalid_cursor",
            "cursor must be the opaque value returned by the previous /tasks response.",
            None,
        ));
    };
    if id.trim().is_empty() {
        return Err(task_list_bad_request(
            "invalid_cursor",
            "cursor task id is empty.",
            None,
        ));
    }
    Ok(TaskListCursor {
        created_at: chrono::DateTime::parse_from_rfc3339(created_at)
            .map_err(|_| {
                task_list_bad_request(
                    "invalid_cursor",
                    "cursor created_at is not a valid RFC3339 timestamp.",
                    None,
                )
            })?
            .with_timezone(&chrono::Utc),
        id: id.to_string(),
    })
}

impl TaskListCursor {
    fn as_task_summary_cursor(&self) -> TaskSummaryPageCursor {
        TaskSummaryPageCursor {
            created_at: self.created_at,
            id: self.id.clone(),
        }
    }
}

fn task_list_bad_request(
    error: &'static str,
    message: impl Into<String>,
    hint: Option<&'static str>,
) -> TaskListQueryError {
    TaskListQueryError {
        error,
        message: message.into(),
        hint,
    }
}

fn allowed_task_statuses() -> Vec<&'static str> {
    vec![
        "pending",
        "awaiting_deps",
        "triaging",
        "planning",
        "implementing",
        "review_generating",
        "review_waiting",
        "planner_generating",
        "planner_waiting",
        "agent_review",
        "waiting",
        "reviewing",
        "done",
        "failed",
        "cancelled",
    ]
}

fn allowed_scheduler_states() -> Vec<&'static str> {
    vec![
        "queued",
        "awaiting_dependencies",
        "running",
        "retry_backoff",
        "leased",
        "recovering",
        "done",
        "failed",
        "cancelled",
    ]
}

fn sort_task_summaries(summaries: &mut [TaskSummary]) {
    summaries.sort_by(compare_task_summaries_desc);
}

fn compare_task_summaries_desc(left: &TaskSummary, right: &TaskSummary) -> Ordering {
    match (
        task_summary_created_at(left),
        task_summary_created_at(right),
    ) {
        (Some(left_created), Some(right_created)) => right_created
            .cmp(&left_created)
            .then_with(|| right.id.as_str().cmp(left.id.as_str())),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => right.id.as_str().cmp(left.id.as_str()),
    }
}

fn task_summary_created_at(summary: &TaskSummary) -> Option<DateTime<Utc>> {
    summary
        .created_at
        .as_deref()
        .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
        .map(|value| value.with_timezone(&Utc))
}

fn task_list_counts(summaries: &[TaskSummary]) -> TaskListCounts {
    let mut by_status = BTreeMap::new();
    let mut by_scheduler_state = BTreeMap::new();
    for summary in summaries {
        *by_status
            .entry(summary.status.as_ref().to_string())
            .or_insert(0) += 1;
        *by_scheduler_state
            .entry(summary.scheduler.authority_state.as_ref().to_string())
            .or_insert(0) += 1;
    }
    TaskListCounts {
        total: summaries.len(),
        running: *by_scheduler_state.get("running").unwrap_or(&0),
        failed: *by_status.get("failed").unwrap_or(&0),
        by_status,
        by_scheduler_state,
    }
}

fn paginate_task_summaries(
    summaries: &[TaskSummary],
    query: &TaskListQuery,
) -> (Vec<TaskSummary>, TaskListPage) {
    let mut page = summaries
        .iter()
        .filter(|summary| {
            query
                .cursor
                .as_ref()
                .is_none_or(|cursor| task_is_after_cursor(summary, cursor))
        })
        .take(query.limit + 1)
        .cloned()
        .collect::<Vec<_>>();
    let has_more = page.len() > query.limit;
    if has_more {
        page.truncate(query.limit);
    }
    let next_cursor = has_more
        .then(|| page.last().and_then(task_list_cursor_for_summary))
        .flatten();
    (
        page,
        TaskListPage {
            limit: query.limit,
            has_more,
            next_cursor,
        },
    )
}

fn task_is_after_cursor(summary: &TaskSummary, cursor: &TaskListCursor) -> bool {
    let Some(created_at) = task_summary_created_at(summary) else {
        return false;
    };
    created_at < cursor.created_at
        || (created_at == cursor.created_at && summary.id.as_str() < cursor.id.as_str())
}

fn task_list_cursor_for_summary(summary: &TaskSummary) -> Option<String> {
    let created_at = summary.created_at.as_deref()?;
    Some(format!("{}|{}", created_at, summary.id.as_str()))
}

async fn append_runtime_submission_summaries(
    state: &AppState,
    summaries: &mut Vec<TaskSummary>,
    filter: &TaskSummaryFilter,
    cursor: Option<&TaskSummaryPageCursor>,
    limit: usize,
) -> anyhow::Result<()> {
    let Some(store) = state.core.workflow_runtime_store.as_ref() else {
        return Ok(());
    };
    append_runtime_definition_summaries(
        store,
        summaries,
        harness_workflow::runtime::GITHUB_ISSUE_PR_DEFINITION_ID,
        TaskKind::Issue,
        filter,
        cursor,
        limit,
    )
    .await?;
    append_runtime_definition_summaries(
        store,
        summaries,
        harness_workflow::runtime::PROMPT_TASK_DEFINITION_ID,
        TaskKind::Prompt,
        filter,
        cursor,
        limit,
    )
    .await
}

async fn append_runtime_definition_summaries(
    store: &harness_workflow::runtime::WorkflowRuntimeStore,
    summaries: &mut Vec<TaskSummary>,
    definition_id: &str,
    task_kind: TaskKind,
    filter: &TaskSummaryFilter,
    cursor: Option<&TaskSummaryPageCursor>,
    limit: usize,
) -> anyhow::Result<()> {
    if !filter.kinds.is_empty() && !filter.kinds.contains(&task_kind) {
        return Ok(());
    }
    let workflows = store
        .list_instances_by_definition_page(
            definition_id,
            filter.project.as_deref(),
            cursor.map(|cursor| cursor.created_at),
            cursor.map(|cursor| cursor.id.as_str()),
            i64::try_from(limit.max(1)).unwrap_or(i64::MAX),
        )
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
        let summary = runtime_workflow_task_summary(workflow, task_id, task_kind);
        if filter.matches_summary(&summary) {
            summaries.push(summary);
        }
    }
    Ok(())
}

fn runtime_workflow_task_summary(
    workflow: harness_workflow::runtime::WorkflowInstance,
    task_id: harness_core::types::TaskId,
    task_kind: TaskKind,
) -> TaskSummary {
    let status = runtime_workflow_state_to_task_status(&workflow.state);
    let scheduler = runtime_workflow_scheduler_state(&workflow.state, &status);
    let issue = workflow
        .data
        .get("issue_number")
        .and_then(|value| value.as_u64());
    let external_id = runtime_external_id(task_kind, &workflow.data, issue);
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

fn runtime_workflow_scheduler_state(state: &str, status: &TaskStatus) -> TaskSchedulerState {
    match state {
        "awaiting_dependencies" => TaskSchedulerState::awaiting_dependencies(),
        "scheduled" | "discovered" => TaskSchedulerState::queued(),
        "implementing" | "replanning" | "addressing_feedback" => TaskSchedulerState {
            authority_state: SchedulerAuthorityState::Running,
            owner: None,
            run_generation: 0,
            recovery_generation: 0,
            lease_expires_at: None,
        },
        "done" | "passed" | "failed" | "cancelled" => {
            let mut scheduler = TaskSchedulerState::queued();
            scheduler.mark_terminal(status);
            scheduler
        }
        "pr_open" | "awaiting_feedback" | "ready_to_merge" | "blocked" => {
            TaskSchedulerState::queued()
        }
        _ => match status {
            TaskStatus::AwaitingDeps => TaskSchedulerState::awaiting_dependencies(),
            TaskStatus::Pending => TaskSchedulerState::queued(),
            TaskStatus::Done | TaskStatus::Failed | TaskStatus::Cancelled => {
                let mut scheduler = TaskSchedulerState::queued();
                scheduler.mark_terminal(status);
                scheduler
            }
            _ => TaskSchedulerState::queued(),
        },
    }
}

fn runtime_string_field(data: &serde_json::Value, field: &str) -> Option<String> {
    data.get(field)
        .and_then(|value| value.as_str())
        .map(ToOwned::to_owned)
}

/// Render the `external_id` shown for a runtime-backed task in dashboard
/// responses. Pulled out of two identical match blocks in
/// [`runtime_workflow_task_summary`] and [`runtime_task_response_by_handle`]
/// so that adding a new `TaskKind` variant only edits one place.
fn runtime_external_id(
    task_kind: TaskKind,
    workflow_data: &serde_json::Value,
    issue: Option<u64>,
) -> Option<String> {
    match task_kind {
        TaskKind::Issue => issue.map(|issue_number| format!("issue:{issue_number}")),
        TaskKind::Prompt => runtime_string_field(workflow_data, "external_id"),
        TaskKind::Pr | TaskKind::Review | TaskKind::Planner => None,
    }
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
    let external_id = runtime_external_id(task_kind, &workflow.data, issue);
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

fn proof_from_runtime_workflow(
    task_id: &harness_core::types::TaskId,
    workflow: &harness_workflow::runtime::WorkflowInstance,
    events: &[harness_workflow::runtime::WorkflowEvent],
    decisions: &[harness_workflow::runtime::WorkflowDecisionRecord],
) -> ProofOfWork {
    let status = runtime_workflow_state_to_task_status(&workflow.state);
    let pr_url = runtime_string_field(&workflow.data, "pr_url")
        .or_else(|| runtime_string_field(&workflow.data, "last_pr_url"));
    let accepted_decisions = decisions
        .iter()
        .filter(|record| record.accepted)
        .collect::<Vec<_>>();
    let approved = events.iter().any(|event| {
        matches!(
            event.event_type.as_str(),
            "PrReadyToMerge" | "MergeApproved" | "PrMerged"
        )
    }) || accepted_decisions.iter().any(|record| {
        matches!(
            record.decision.decision.as_str(),
            "mark_ready_to_merge" | "approve_merge" | "record_pr_merged" | "quality_passed"
        )
    }) || workflow.state == "passed";
    let changes_requested = events
        .iter()
        .any(|event| event.event_type == "FeedbackFound")
        || accepted_decisions.iter().any(|record| {
            matches!(
                record.decision.decision.as_str(),
                "address_pr_feedback" | "await_feedback_after_rework"
            )
        });

    let review_outcome = if approved {
        ReviewOutcome::Approved
    } else if changes_requested {
        ReviewOutcome::ChangesRequested
    } else {
        ReviewOutcome::Skipped
    };
    let ci_status = if status.is_failure() {
        CiStatus::Failed
    } else if status == TaskStatus::Done && review_outcome == ReviewOutcome::Approved {
        CiStatus::Passed
    } else {
        CiStatus::Unknown
    };
    let review_event_count = events
        .iter()
        .filter(|event| {
            matches!(
                event.event_type.as_str(),
                "FeedbackFound"
                    | "NoFeedbackFound"
                    | "PrReadyToMerge"
                    | "MergeApproved"
                    | "PrMerged"
            )
        })
        .count();
    let review_decision_count = accepted_decisions
        .iter()
        .filter(|record| {
            matches!(
                record.decision.decision.as_str(),
                "address_pr_feedback"
                    | "wait_for_pr_feedback"
                    | "mark_ready_to_merge"
                    | "approve_merge"
                    | "record_pr_merged"
                    | "quality_passed"
            )
        })
        .count();
    let review_rounds = review_event_count.max(review_decision_count) as u32;

    let mut signals = vec![
        QualitySignal {
            name: "workflow_id".to_string(),
            value: workflow.id.clone(),
        },
        QualitySignal {
            name: "workflow_state".to_string(),
            value: workflow.state.clone(),
        },
    ];
    if let Some(error) = runtime_string_field(&workflow.data, "failure_reason") {
        signals.push(QualitySignal {
            name: "error".to_string(),
            value: error,
        });
    }

    ProofOfWork {
        task_id: task_id.as_str().to_string(),
        status: status.as_ref().to_string(),
        pr_url,
        ci_status,
        review_outcome,
        review_rounds,
        quality_signals: signals,
    }
}

async fn runtime_proof_by_handle(
    state: &AppState,
    task_id: &harness_core::types::TaskId,
) -> anyhow::Result<RuntimeProofLookup> {
    let Some(store) = state.core.workflow_runtime_store.as_ref() else {
        return Ok(RuntimeProofLookup::Missing);
    };
    let Some(workflow) = store.get_instance_by_task_id(task_id.as_str()).await? else {
        return Ok(RuntimeProofLookup::Missing);
    };
    let status = runtime_workflow_state_to_task_status(&workflow.state);
    if !status.is_terminal() {
        return Ok(RuntimeProofLookup::InFlight(status.as_ref().to_string()));
    }
    let events = store.events_for(&workflow.id).await?;
    let decisions = store.decisions_for(&workflow.id).await?;
    Ok(RuntimeProofLookup::Terminal(proof_from_runtime_workflow(
        task_id, &workflow, &events, &decisions,
    )))
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
        Ok(None) => match runtime_proof_by_handle(&state, &task_id).await {
            Ok(RuntimeProofLookup::Terminal(proof)) => Json(proof).into_response(),
            Ok(RuntimeProofLookup::InFlight(status)) => (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(json!({
                    "error": "task is not in a terminal state",
                    "status": status,
                })),
            )
                .into_response(),
            Ok(RuntimeProofLookup::Missing) => (
                StatusCode::NOT_FOUND,
                Json(json!({"error": "task not found"})),
            )
                .into_response(),
            Err(e) => {
                tracing::error!("get_task_proof: runtime workflow lookup failed: {e}");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({"error": "internal server error"})),
                )
                    .into_response()
            }
        },
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
