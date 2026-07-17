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
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use super::state::AppState;
use crate::task_runner::{
    SchedulerAuthorityState, TaskId, TaskKind, TaskState, TaskStatus, TaskSummary,
    TaskSummaryFilter, TaskSummaryPageCursor, TaskTerminalInfo, TaskWorkflowSummary,
};
use harness_core::proof_of_work::{CiStatus, ProofOfWork, QualitySignal, ReviewOutcome};

mod detail;
mod runtime_submissions;
pub(crate) use detail::{
    get_runtime_submission, get_runtime_submission_proof, get_task, get_task_artifacts,
    get_task_prompts, get_task_proof,
};
use runtime_submissions::append_runtime_submission_summaries;
pub(super) use runtime_submissions::{
    runtime_external_id, runtime_submission_task_kind, runtime_task_id_array,
    workflow_runtime_store_required,
};

#[cfg(test)]
pub(crate) use detail::{proof_from_runtime_workflow, proof_from_state};

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
    data: Vec<TaskSummaryResponse>,
    page: TaskListPage,
    counts: TaskListCounts,
    #[serde(skip_serializing_if = "Option::is_none")]
    degraded: Option<TaskListDegradation>,
}

#[derive(Serialize)]
struct TaskSummaryResponse {
    #[serde(flatten)]
    inner: TaskSummary,
    #[serde(skip_serializing_if = "Option::is_none")]
    terminal: Option<TaskTerminalInfo>,
}

impl From<TaskSummary> for TaskSummaryResponse {
    fn from(summary: TaskSummary) -> Self {
        let terminal =
            TaskTerminalInfo::from_status_error(&summary.status, summary.error.as_deref());
        Self {
            inner: summary,
            terminal,
        }
    }
}

#[derive(Serialize)]
struct TaskListDegradation {
    partial: bool,
    missing: Vec<&'static str>,
    reason: &'static str,
}

impl TaskListDegradation {
    fn runtime_submission_summaries(reason: &'static str) -> Self {
        Self {
            partial: true,
            missing: vec!["workflow_runtime_submissions"],
            reason,
        }
    }
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
            let degraded = if let Err(e) = append_runtime_submission_summaries(
                &state,
                &mut summaries,
                &query.filter,
                task_store_cursor.as_ref(),
                query.limit + 1,
            )
            .await
            {
                tracing::error!("list_tasks: failed to append runtime submission summaries: {e}");
                Some(TaskListDegradation::runtime_submission_summaries(
                    "runtime_submission_summaries_unavailable",
                ))
            } else {
                None
            };
            sort_task_summaries(&mut summaries);
            let (data, page) = paginate_task_summaries(&summaries, &query);
            let counts = task_list_counts(&data);
            Json(TaskListResponse {
                data: data.into_iter().map(TaskSummaryResponse::from).collect(),
                page,
                counts,
                degraded,
            })
            .into_response()
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

/// List workflow-runtime submissions without consulting the legacy task store.
pub(crate) async fn list_runtime_submissions(
    State(state): State<Arc<AppState>>,
    query: Result<Query<RawTaskListParams>, QueryRejection>,
) -> Response {
    let raw_query = match query {
        Ok(Query(query)) => query,
        Err(error) => {
            return task_list_bad_request(
                "invalid_query",
                format!("Invalid runtime submission query parameters: {error}"),
                None,
            )
            .into_response();
        }
    };
    let query = match TaskListQuery::from_raw(raw_query) {
        Ok(query) => query,
        Err(error) => return error.into_response(),
    };
    if state.core.workflow_runtime_store.is_none() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error": "workflow runtime store unavailable"})),
        )
            .into_response();
    }

    let cursor = query
        .cursor
        .as_ref()
        .map(TaskListCursor::as_task_summary_cursor);
    let mut summaries = Vec::new();
    if let Err(error) = append_runtime_submission_summaries(
        &state,
        &mut summaries,
        &query.filter,
        cursor.as_ref(),
        query.limit + 1,
    )
    .await
    {
        tracing::error!(
            "list_runtime_submissions: failed to load runtime submission summaries: {error}"
        );
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": "internal server error"})),
        )
            .into_response();
    }

    sort_task_summaries(&mut summaries);
    let (data, page) = paginate_task_summaries(&summaries, &query);
    let counts = task_list_counts(&data);
    Json(TaskListResponse {
        data: data.into_iter().map(TaskSummaryResponse::from).collect(),
        page,
        counts,
        degraded: None,
    })
    .into_response()
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

#[cfg(test)]
#[path = "task_query_routes_tests.rs"]
mod tests;
