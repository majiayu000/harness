use axum::{
    extract::{rejection::QueryRejection, Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{cmp::Ordering, collections::BTreeMap, sync::Arc};

use super::state::AppState;
use crate::workflow_runtime_submission::{
    runtime_models::{TaskId, TaskKind, TaskStatus, TaskTerminalInfo},
    runtime_state::{
        RuntimeSubmissionSummaryCursor, RuntimeSubmissionSummaryFilter, SchedulerAuthorityState,
        TaskSummary, TaskWorkflowSummary,
    },
};
use harness_core::proof_of_work::{CiStatus, ProofOfWork, QualitySignal, ReviewOutcome};

mod detail;
mod runtime_submissions;
pub(crate) use detail::{get_runtime_submission, get_runtime_submission_proof};
use runtime_submissions::append_runtime_submission_summaries;
pub(super) use runtime_submissions::{
    runtime_external_id, runtime_submission_task_kind, runtime_task_id_array,
};

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub(crate) struct RuntimeSubmissionListParams {
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
struct RuntimeSubmissionListQuery {
    filter: RuntimeSubmissionSummaryFilter,
    limit: usize,
    cursor: Option<RuntimeSubmissionListCursor>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RuntimeSubmissionListCursor {
    created_at: DateTime<Utc>,
    id: String,
}

#[derive(Serialize)]
struct RuntimeSubmissionListResponse {
    data: Vec<RuntimeSubmissionSummaryResponse>,
    page: RuntimeSubmissionListPage,
    counts: RuntimeSubmissionListCounts,
}

#[derive(Serialize)]
struct RuntimeSubmissionSummaryResponse {
    #[serde(flatten)]
    inner: TaskSummary,
    #[serde(skip_serializing_if = "Option::is_none")]
    terminal: Option<TaskTerminalInfo>,
}

impl From<TaskSummary> for RuntimeSubmissionSummaryResponse {
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
struct RuntimeSubmissionListPage {
    limit: usize,
    has_more: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    next_cursor: Option<String>,
}

#[derive(Serialize)]
struct RuntimeSubmissionListCounts {
    total: usize,
    running: usize,
    failed: usize,
    by_status: BTreeMap<String, usize>,
    by_scheduler_state: BTreeMap<String, usize>,
}

const DEFAULT_RUNTIME_SUBMISSION_LIST_LIMIT: u32 = 50;
const MAX_RUNTIME_SUBMISSION_LIST_LIMIT: u32 = 500;

#[derive(Debug)]
struct RuntimeSubmissionListQueryError {
    error: &'static str,
    message: String,
    hint: Option<&'static str>,
}

impl IntoResponse for RuntimeSubmissionListQueryError {
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
                    "kind": ["issue", "pr", "prompt"],
                }
            })),
        )
            .into_response()
    }
}

pub(crate) async fn list_runtime_submissions(
    State(state): State<Arc<AppState>>,
    query: Result<Query<RuntimeSubmissionListParams>, QueryRejection>,
) -> Response {
    let raw_query = match query {
        Ok(Query(query)) => query,
        Err(error) => {
            return list_bad_request(
                "invalid_query",
                format!("Invalid runtime submission query parameters: {error}"),
                None,
            )
            .into_response();
        }
    };
    let query = match RuntimeSubmissionListQuery::from_raw(raw_query) {
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
        .map(|cursor| cursor.as_runtime_cursor());
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

    sort_runtime_submission_summaries(&mut summaries);
    let (data, page) = paginate_runtime_submission_summaries(&summaries, &query);
    let counts = runtime_submission_list_counts(&data);
    Json(RuntimeSubmissionListResponse {
        data: data
            .into_iter()
            .map(RuntimeSubmissionSummaryResponse::from)
            .collect(),
        page,
        counts,
    })
    .into_response()
}

impl RuntimeSubmissionListQuery {
    fn from_raw(raw: RuntimeSubmissionListParams) -> Result<Self, RuntimeSubmissionListQueryError> {
        let statuses = parse_task_statuses(raw.status.as_deref())?;
        let scheduler_states = parse_scheduler_states(raw.scheduler_state.as_deref())?;
        let active = raw.active.unwrap_or(false);
        if active && statuses.iter().any(TaskStatus::is_terminal) {
            return Err(list_bad_request(
                "invalid_filter",
                "`active=true` cannot be combined with terminal task statuses.",
                Some("Remove active=true or query terminal statuses without the active filter."),
            ));
        }
        let limit = raw.limit.unwrap_or(DEFAULT_RUNTIME_SUBMISSION_LIST_LIMIT);
        if !(1..=MAX_RUNTIME_SUBMISSION_LIST_LIMIT).contains(&limit) {
            return Err(list_bad_request(
                "invalid_limit",
                format!("limit must be between 1 and {MAX_RUNTIME_SUBMISSION_LIST_LIMIT}."),
                None,
            ));
        }
        Ok(Self {
            filter: RuntimeSubmissionSummaryFilter {
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
                .map(parse_runtime_submission_list_cursor)
                .transpose()?,
        })
    }
}

fn parse_task_statuses(
    raw: Option<&str>,
) -> Result<Vec<TaskStatus>, RuntimeSubmissionListQueryError> {
    parse_csv(raw)
        .into_iter()
        .map(|value| {
            value.parse::<TaskStatus>().map_err(|_| {
                let hint = (value == "running")
                    .then_some("Use scheduler_state=running instead of status=running.");
                list_bad_request(
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
) -> Result<Vec<SchedulerAuthorityState>, RuntimeSubmissionListQueryError> {
    parse_csv(raw)
        .into_iter()
        .map(|value| {
            value.parse::<SchedulerAuthorityState>().map_err(|_| {
                list_bad_request(
                    "invalid_scheduler_state",
                    format!("Unknown scheduler_state `{value}`."),
                    None,
                )
            })
        })
        .collect()
}

fn parse_task_kinds(raw: Option<&str>) -> Result<Vec<TaskKind>, RuntimeSubmissionListQueryError> {
    parse_csv(raw)
        .into_iter()
        .map(|value| match value.as_str() {
            "issue" => Ok(TaskKind::Issue),
            "pr" => Ok(TaskKind::Pr),
            "prompt" => Ok(TaskKind::Prompt),
            _ => Err(list_bad_request(
                "invalid_kind",
                format!("Unknown runtime submission kind `{value}`."),
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

fn parse_runtime_submission_list_cursor(
    raw: &str,
) -> Result<RuntimeSubmissionListCursor, RuntimeSubmissionListQueryError> {
    let Some((created_at, id)) = raw.split_once('|') else {
        return Err(list_bad_request(
            "invalid_cursor",
            "cursor must be the opaque value returned by the previous runtime submission response.",
            None,
        ));
    };
    if id.trim().is_empty() {
        return Err(list_bad_request(
            "invalid_cursor",
            "cursor submission id is empty.",
            None,
        ));
    }
    Ok(RuntimeSubmissionListCursor {
        created_at: DateTime::parse_from_rfc3339(created_at)
            .map_err(|_| {
                list_bad_request(
                    "invalid_cursor",
                    "cursor created_at is not a valid RFC3339 timestamp.",
                    None,
                )
            })?
            .with_timezone(&Utc),
        id: id.to_string(),
    })
}

impl RuntimeSubmissionListCursor {
    fn as_runtime_cursor(&self) -> RuntimeSubmissionSummaryCursor {
        RuntimeSubmissionSummaryCursor {
            created_at: self.created_at,
            id: self.id.clone(),
        }
    }
}

fn list_bad_request(
    error: &'static str,
    message: impl Into<String>,
    hint: Option<&'static str>,
) -> RuntimeSubmissionListQueryError {
    RuntimeSubmissionListQueryError {
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

fn sort_runtime_submission_summaries(summaries: &mut [TaskSummary]) {
    summaries.sort_by(compare_runtime_submission_summaries_desc);
}

fn compare_runtime_submission_summaries_desc(left: &TaskSummary, right: &TaskSummary) -> Ordering {
    match (
        runtime_submission_summary_created_at(left),
        runtime_submission_summary_created_at(right),
    ) {
        (Some(left_created), Some(right_created)) => right_created
            .cmp(&left_created)
            .then_with(|| right.id.as_str().cmp(left.id.as_str())),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => right.id.as_str().cmp(left.id.as_str()),
    }
}

fn runtime_submission_summary_created_at(summary: &TaskSummary) -> Option<DateTime<Utc>> {
    summary
        .created_at
        .as_deref()
        .and_then(|value| DateTime::parse_from_rfc3339(value).ok())
        .map(|value| value.with_timezone(&Utc))
}

fn runtime_submission_list_counts(summaries: &[TaskSummary]) -> RuntimeSubmissionListCounts {
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
    RuntimeSubmissionListCounts {
        total: summaries.len(),
        running: *by_scheduler_state.get("running").unwrap_or(&0),
        failed: *by_status.get("failed").unwrap_or(&0),
        by_status,
        by_scheduler_state,
    }
}

fn paginate_runtime_submission_summaries(
    summaries: &[TaskSummary],
    query: &RuntimeSubmissionListQuery,
) -> (Vec<TaskSummary>, RuntimeSubmissionListPage) {
    let mut page = summaries
        .iter()
        .filter(|summary| {
            query
                .cursor
                .as_ref()
                .is_none_or(|cursor| runtime_submission_is_after_cursor(summary, cursor))
        })
        .take(query.limit + 1)
        .cloned()
        .collect::<Vec<_>>();
    let has_more = page.len() > query.limit;
    if has_more {
        page.truncate(query.limit);
    }
    let next_cursor = has_more
        .then(|| page.last().and_then(runtime_submission_cursor_for_summary))
        .flatten();
    (
        page,
        RuntimeSubmissionListPage {
            limit: query.limit,
            has_more,
            next_cursor,
        },
    )
}

fn runtime_submission_is_after_cursor(
    summary: &TaskSummary,
    cursor: &RuntimeSubmissionListCursor,
) -> bool {
    let Some(created_at) = runtime_submission_summary_created_at(summary) else {
        return false;
    };
    created_at < cursor.created_at
        || (created_at == cursor.created_at && summary.id.as_str() < cursor.id.as_str())
}

fn runtime_submission_cursor_for_summary(summary: &TaskSummary) -> Option<String> {
    let created_at = summary.created_at.as_deref()?;
    Some(format!("{}|{}", created_at, summary.id.as_str()))
}
