use axum::{
    body::Bytes,
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use harness_protocol::methods::RpcRequest;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path as StdPath, PathBuf};
use std::sync::Arc;

use super::{state::AppState, task_routes};
use crate::runtime_projection::RuntimeWorkflowProjection;
use crate::{router, task_runner};
use harness_workflow::runtime::store::{RuntimeEventSummary, RuntimeJobCompactRecord};
use harness_workflow::runtime::{
    runtime_job_running_lease_state_at, RuntimeEvent, RuntimeJob, RuntimeJobStatus,
    WorkflowCommand, WorkflowCommandRecord, WorkflowDecisionRecord, WorkflowEvent,
    WorkflowInstance, WorkflowLease,
};

const ACTIVITY_RESULT_ENVELOPE_ARTIFACT_TYPE: &str = "activity_result_envelope";
const ACTIVITY_RESULT_ENVELOPE_SCHEMA: &str = "harness.runtime.activity_result_envelope.v1";
const WORKFLOW_RUNTIME_TREE_DEFAULT_LIMIT: i64 = 100;
const WORKFLOW_RUNTIME_TREE_MAX_LIMIT: i64 = 500;
const WORKFLOW_RUNTIME_TREE_DEFAULT_JOB_LIMIT: i64 = 5;
const WORKFLOW_RUNTIME_TREE_MAX_JOB_LIMIT: i64 = 50;
const WORKFLOW_RUNTIME_TREE_DEFAULT_COMMAND_LIMIT: i64 = 5;
const WORKFLOW_RUNTIME_TREE_MAX_COMMAND_LIMIT: i64 = 50;
const WORKFLOW_RUNTIME_TREE_DEFAULT_REJECTED_DECISION_LIMIT: i64 = 3;
const WORKFLOW_RUNTIME_TREE_MAX_REJECTED_DECISION_LIMIT: i64 = 20;

#[derive(Debug)]
struct IntakeRecentDispatch {
    sort_at: Option<chrono::DateTime<chrono::Utc>>,
    payload: serde_json::Value,
}

fn startup_error_code(error: Option<&str>) -> Option<&'static str> {
    let error = error?;
    let lower = error.to_ascii_lowercase();
    if lower.contains("migration") {
        Some("migration_failed")
    } else if lower.contains("timeout") || lower.contains("timed out") {
        Some("timeout")
    } else if lower.contains("connection")
        || lower.contains("connect")
        || lower.contains("database")
        || lower.contains("postgres")
        || lower.contains("pool")
    {
        Some("database_unavailable")
    } else {
        Some("startup_failed")
    }
}

pub(crate) async fn health_check(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let count = state.core.tasks.count();
    let dirty = state.is_runtime_state_dirty();
    let degraded = &state.degraded_subsystems;
    let runtime_logs = &state.core.server.runtime_logs;
    let startup_statuses: Vec<serde_json::Value> = state
        .startup_statuses
        .iter()
        .map(|status| {
            json!({
                "name": status.name,
                "critical": status.is_critical(),
                "ready": status.ready,
                "error": startup_error_code(status.error.as_deref()),
            })
        })
        .collect();
    let status = if degraded.is_empty() && !dirty {
        "ok"
    } else {
        "degraded"
    };
    Json(json!({
        "status": status,
        "tasks": count,
        "persistence": {
            "degraded_subsystems": degraded,
            "runtime_state_dirty": dirty,
            "startup": {
                "stores": startup_statuses,
            }
        },
        "runtime_logs": {
            "state": runtime_logs.state.as_str(),
            "path_hint": runtime_logs.path_hint.clone(),
            "retention_days": runtime_logs.retention_days,
        }
    }))
}

/// GET /projects/queue-stats — per-project queue stats alongside the global queue summary.
pub(crate) async fn project_queue_stats(
    State(state): State<Arc<AppState>>,
) -> Json<serde_json::Value> {
    let tq = &state.concurrency.task_queue;
    let projects: serde_json::Map<String, serde_json::Value> = tq
        .all_project_stats()
        .into_iter()
        .map(|(id, s)| {
            (
                id,
                json!({
                    "running": s.running,
                    "queued": s.queued,
                    "limit": s.limit,
                }),
            )
        })
        .collect();
    Json(json!({
        "global": {
            "running": tq.running_count(),
            "queued": tq.queued_count(),
            "limit": tq.global_limit(),
        },
        "projects": projects,
    }))
}

#[derive(Debug, serde::Deserialize)]
pub(crate) struct IssueWorkflowByIssueQuery {
    pub project_id: String,
    pub repo: Option<String>,
    pub issue: u64,
}

#[derive(Debug, serde::Deserialize)]
pub(crate) struct IssueWorkflowByPrQuery {
    pub project_id: String,
    pub repo: Option<String>,
    pub pr: u64,
}

#[derive(Debug, serde::Deserialize)]
pub(crate) struct ProjectWorkflowByProjectQuery {
    pub project_id: String,
    pub repo: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
pub(crate) struct WorkflowRuntimeTreeQuery {
    pub project_id: Option<String>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
    pub job_limit: Option<i64>,
    pub command_limit: Option<i64>,
    pub rejected_decision_limit: Option<i64>,
    pub summary_only: Option<bool>,
    pub detail: Option<WorkflowRuntimeTreeDetail>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum WorkflowRuntimeTreeDetail {
    Compact,
    Full,
}

impl WorkflowRuntimeTreeDetail {
    fn as_str(self) -> &'static str {
        match self {
            Self::Compact => "compact",
            Self::Full => "full",
        }
    }
}

#[derive(Debug, Clone, serde::Serialize)]
struct WorkflowRuntimeCommandNode {
    pub id: String,
    pub workflow_id: String,
    pub decision_id: Option<String>,
    pub status: String,
    pub command: WorkflowCommand,
    pub runtime_job_count: usize,
    pub runtime_jobs: Vec<WorkflowRuntimeJobNode>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

impl WorkflowRuntimeCommandNode {
    fn new(
        record: WorkflowCommandRecord,
        runtime_job_count: usize,
        runtime_jobs: Vec<WorkflowRuntimeJobNode>,
    ) -> Self {
        Self {
            id: record.id,
            workflow_id: record.workflow_id,
            decision_id: record.decision_id,
            status: record.status.to_string(),
            command: record.command,
            runtime_job_count,
            runtime_jobs,
            created_at: record.created_at,
            updated_at: record.updated_at,
        }
    }
}

#[derive(Debug, Clone, serde::Serialize)]
struct WorkflowRuntimeJobNode {
    pub id: String,
    pub command_id: String,
    pub runtime_kind: harness_workflow::runtime::RuntimeKind,
    pub runtime_profile: String,
    pub status: RuntimeJobStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lease: Option<WorkflowLease>,
    pub lease_generation: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub not_before: Option<chrono::DateTime<chrono::Utc>>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
    pub runtime_event_count: usize,
    pub latest_runtime_event_type: Option<String>,
    pub prompt_packet_digest: Option<String>,
    pub activity_result_envelope: Option<Value>,
    pub lease_state: Option<&'static str>,
    pub in_flight_model_turn: bool,
    pub last_runtime_observation_at: Option<chrono::DateTime<chrono::Utc>>,
}

impl WorkflowRuntimeJobNode {
    fn full(job: RuntimeJob, runtime_events: Vec<RuntimeEvent>) -> Self {
        let latest_runtime_event_type = runtime_events.last().map(|event| event.event_type.clone());
        let prompt_packet_digest = prompt_packet_digest_from_events(&runtime_events);
        let activity_result_envelope = activity_result_envelope_from_job(&job);
        let lease_state = runtime_job_running_lease_state_at(&job, chrono::Utc::now())
            .map(|state| state.status_label());
        let in_flight_model_turn = runtime_job_has_in_flight_model_turn(&job, &runtime_events);
        let last_runtime_observation_at = last_runtime_observation_at(&job, &runtime_events);
        Self {
            id: job.id,
            command_id: job.command_id,
            runtime_kind: job.runtime_kind,
            runtime_profile: job.runtime_profile,
            status: job.status,
            lease: job.lease,
            lease_generation: job.lease_generation,
            input: Some(job.input),
            output: job.output,
            error: job.error,
            not_before: job.not_before,
            created_at: job.created_at,
            updated_at: job.updated_at,
            runtime_event_count: runtime_events.len(),
            latest_runtime_event_type,
            prompt_packet_digest,
            activity_result_envelope,
            lease_state,
            in_flight_model_turn,
            last_runtime_observation_at,
        }
    }

    fn compact(job: RuntimeJobCompactRecord, runtime_summary: RuntimeEventSummary) -> Self {
        let now = chrono::Utc::now();
        let lease_state = if job.status == RuntimeJobStatus::Running {
            Some(match job.lease.as_ref() {
                Some(lease) if lease.expires_at > now => "active_leased",
                Some(_) => "expired_lease",
                None => "missing_lease",
            })
        } else {
            None
        };
        let latest_turn_sequence = runtime_summary.latest_turn_sequence.unwrap_or(0);
        let latest_activity_result_sequence =
            runtime_summary.latest_activity_result_sequence.unwrap_or(0);
        let in_flight_model_turn = job.status == RuntimeJobStatus::Running
            && latest_turn_sequence > latest_activity_result_sequence;
        let last_runtime_observation_at = if job.status == RuntimeJobStatus::Running {
            Some(
                runtime_summary
                    .latest_runtime_event_at
                    .map(|created_at| created_at.max(job.updated_at))
                    .unwrap_or(job.updated_at),
            )
        } else {
            runtime_summary.latest_runtime_event_at
        };
        Self {
            id: job.id,
            command_id: job.command_id,
            runtime_kind: job.runtime_kind,
            runtime_profile: job.runtime_profile,
            status: job.status,
            lease: job.lease,
            lease_generation: job.lease_generation,
            input: None,
            output: None,
            error: job.error,
            not_before: job.not_before,
            created_at: job.created_at,
            updated_at: job.updated_at,
            runtime_event_count: runtime_summary.runtime_event_count,
            latest_runtime_event_type: runtime_summary.latest_runtime_event_type,
            prompt_packet_digest: runtime_summary.prompt_packet_digest,
            activity_result_envelope: None,
            lease_state,
            in_flight_model_turn,
            last_runtime_observation_at,
        }
    }
}

#[derive(Debug, Clone, serde::Serialize)]
struct WorkflowRuntimeTreeNode {
    pub workflow: WorkflowInstance,
    pub runtime_job_count: usize,
    pub event_count: usize,
    pub decision_count: usize,
    pub rejected_decision_count: usize,
    pub command_count: usize,
    pub events: Vec<WorkflowEvent>,
    pub decisions: Vec<WorkflowDecisionRecord>,
    pub commands: Vec<WorkflowRuntimeCommandNode>,
    pub children: Vec<WorkflowRuntimeTreeNode>,
}

#[derive(Debug, serde::Serialize)]
struct WorkflowRuntimeTreeResponse {
    pub workflows: Vec<WorkflowRuntimeTreeNode>,
    pub total_workflows: usize,
    pub pagination: WorkflowRuntimeTreePagination,
    pub summary: WorkflowRuntimeTreeSummary,
}

#[derive(Debug, serde::Serialize)]
struct WorkflowRuntimeTreePagination {
    pub limit: usize,
    pub offset: usize,
    pub returned: usize,
    pub total: usize,
    pub has_more: bool,
    pub next_offset: Option<usize>,
    pub job_limit: usize,
    pub command_limit: Option<usize>,
    pub detail: &'static str,
    pub summary_only: bool,
}

impl WorkflowRuntimeTreePagination {
    fn new(
        limit: i64,
        offset: i64,
        returned: usize,
        total: i64,
        job_limit: usize,
        command_limit: Option<usize>,
        detail: WorkflowRuntimeTreeDetail,
        summary_only: bool,
    ) -> Self {
        let limit = limit.max(1) as usize;
        let offset = offset.max(0) as usize;
        let total = total.max(0) as usize;
        let next_offset = offset.saturating_add(returned);
        let has_more = !summary_only && next_offset < total;
        Self {
            limit,
            offset,
            returned,
            total,
            has_more,
            next_offset: has_more.then_some(next_offset),
            job_limit,
            command_limit,
            detail: detail.as_str(),
            summary_only,
        }
    }
}

#[derive(Debug, Default, serde::Serialize)]
struct WorkflowRuntimeTreeSummary {
    pub total_commands: usize,
    pub total_runtime_jobs: usize,
    pub command_statuses: BTreeMap<String, usize>,
    pub runtime_job_statuses: BTreeMap<String, usize>,
    pub running_job_lease_statuses: BTreeMap<String, usize>,
    pub activity_outcomes: BTreeMap<String, usize>,
    pub jobs_without_activity_envelope: usize,
}

#[derive(Debug, Clone, Copy)]
enum WorkflowRuntimeTreeMode {
    SummaryOnly,
    Compact {
        command_limit: usize,
        rejected_decision_limit: usize,
    },
    Full,
}

impl WorkflowRuntimeTreeMode {
    fn detail(self) -> WorkflowRuntimeTreeDetail {
        match self {
            Self::Full => WorkflowRuntimeTreeDetail::Full,
            Self::SummaryOnly | Self::Compact { .. } => WorkflowRuntimeTreeDetail::Compact,
        }
    }

    fn summary_only(self) -> bool {
        matches!(self, Self::SummaryOnly)
    }

    fn command_limit(self) -> Option<usize> {
        match self {
            Self::Compact { command_limit, .. } => Some(command_limit),
            Self::SummaryOnly | Self::Full => None,
        }
    }
}

pub(crate) async fn get_issue_workflow_by_issue(
    State(state): State<Arc<AppState>>,
    Query(query): Query<IssueWorkflowByIssueQuery>,
) -> Response {
    let Some(store) = state.core.issue_workflow_store.as_ref() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "error": "issue workflow store unavailable" })),
        )
            .into_response();
    };
    match store
        .get_by_issue(&query.project_id, query.repo.as_deref(), query.issue)
        .await
    {
        Ok(Some(workflow)) => (StatusCode::OK, Json(json!(workflow))).into_response(),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "issue workflow not found" })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

pub(crate) async fn get_issue_workflow_by_pr(
    State(state): State<Arc<AppState>>,
    Query(query): Query<IssueWorkflowByPrQuery>,
) -> Response {
    let Some(store) = state.core.issue_workflow_store.as_ref() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "error": "issue workflow store unavailable" })),
        )
            .into_response();
    };
    match store
        .get_by_pr(&query.project_id, query.repo.as_deref(), query.pr)
        .await
    {
        Ok(Some(workflow)) => (StatusCode::OK, Json(json!(workflow))).into_response(),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "issue workflow not found" })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

pub(crate) async fn get_project_workflow_by_project(
    State(state): State<Arc<AppState>>,
    Query(query): Query<ProjectWorkflowByProjectQuery>,
) -> Response {
    let Some(store) = state.core.project_workflow_store.as_ref() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "error": "project workflow store unavailable" })),
        )
            .into_response();
    };
    match store
        .get_by_project(&query.project_id, query.repo.as_deref())
        .await
    {
        Ok(Some(workflow)) => (StatusCode::OK, Json(json!(workflow))).into_response(),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "project workflow not found" })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

pub(crate) async fn get_workflow_runtime_tree(
    State(state): State<Arc<AppState>>,
    Query(query): Query<WorkflowRuntimeTreeQuery>,
) -> Response {
    let Some(store) = state.core.workflow_runtime_store.as_ref() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "error": "workflow runtime store unavailable" })),
        )
            .into_response();
    };
    let limit = query
        .limit
        .unwrap_or(WORKFLOW_RUNTIME_TREE_DEFAULT_LIMIT)
        .clamp(1, WORKFLOW_RUNTIME_TREE_MAX_LIMIT);
    let offset = query.offset.unwrap_or(0).max(0);
    let job_limit = query
        .job_limit
        .unwrap_or(WORKFLOW_RUNTIME_TREE_DEFAULT_JOB_LIMIT)
        .clamp(0, WORKFLOW_RUNTIME_TREE_MAX_JOB_LIMIT);
    let command_limit = query
        .command_limit
        .unwrap_or(WORKFLOW_RUNTIME_TREE_DEFAULT_COMMAND_LIMIT)
        .clamp(0, WORKFLOW_RUNTIME_TREE_MAX_COMMAND_LIMIT);
    let rejected_decision_limit = query
        .rejected_decision_limit
        .unwrap_or(WORKFLOW_RUNTIME_TREE_DEFAULT_REJECTED_DECISION_LIMIT)
        .clamp(0, WORKFLOW_RUNTIME_TREE_MAX_REJECTED_DECISION_LIMIT);
    let detail = query.detail.unwrap_or(WorkflowRuntimeTreeDetail::Compact);
    let mode = if query.summary_only.unwrap_or(false) {
        WorkflowRuntimeTreeMode::SummaryOnly
    } else {
        match detail {
            WorkflowRuntimeTreeDetail::Compact => WorkflowRuntimeTreeMode::Compact {
                command_limit: command_limit as usize,
                rejected_decision_limit: rejected_decision_limit as usize,
            },
            WorkflowRuntimeTreeDetail::Full => WorkflowRuntimeTreeMode::Full,
        }
    };
    match store
        .list_instances_page(query.project_id.as_deref(), limit, offset)
        .await
    {
        Ok(page) => match build_workflow_runtime_tree(
            store,
            page,
            query.project_id.as_deref(),
            job_limit as usize,
            mode,
        )
        .await
        {
            Ok(response) => (StatusCode::OK, Json(response)).into_response(),
            Err(e) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e.to_string() })),
            )
                .into_response(),
        },
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

async fn build_workflow_runtime_tree(
    store: &harness_workflow::runtime::WorkflowRuntimeStore,
    page: harness_workflow::runtime::store::WorkflowInstancePage,
    project_id: Option<&str>,
    job_limit: usize,
    mode: WorkflowRuntimeTreeMode,
) -> anyhow::Result<WorkflowRuntimeTreeResponse> {
    let instances = page.instances;
    let workflow_ids: Vec<String> = instances
        .iter()
        .map(|instance| instance.id.clone())
        .collect();
    let summary = workflow_runtime_tree_summary(store, project_id).await?;
    if mode.summary_only() {
        return Ok(WorkflowRuntimeTreeResponse {
            total_workflows: page.total.max(0) as usize,
            pagination: WorkflowRuntimeTreePagination::new(
                page.limit,
                page.offset,
                0,
                page.total,
                job_limit,
                mode.command_limit(),
                mode.detail(),
                true,
            ),
            summary,
            workflows: Vec::new(),
        });
    }

    let (by_id, children_by_parent) = match mode {
        WorkflowRuntimeTreeMode::Full => {
            build_full_workflow_runtime_nodes(store, instances, &workflow_ids, job_limit).await?
        }
        WorkflowRuntimeTreeMode::Compact {
            command_limit,
            rejected_decision_limit,
        } => {
            build_compact_workflow_runtime_nodes(
                store,
                instances,
                &workflow_ids,
                job_limit,
                command_limit,
                rejected_decision_limit,
            )
            .await?
        }
        WorkflowRuntimeTreeMode::SummaryOnly => unreachable!("summary-only returned above"),
    };

    let mut root_ids = Vec::new();
    for (workflow_id, node) in &by_id {
        let parent_present = node
            .workflow
            .parent_workflow_id
            .as_ref()
            .is_some_and(|parent_id| by_id.contains_key(parent_id));
        if !parent_present {
            root_ids.push(workflow_id.clone());
        }
    }
    if root_ids.is_empty() {
        root_ids.extend(by_id.keys().cloned());
    }

    let mut workflows = Vec::new();
    for root_id in root_ids {
        let mut path = BTreeSet::new();
        if let Some(node) =
            attach_workflow_children(&root_id, &by_id, &children_by_parent, &mut path)
        {
            workflows.push(node);
        }
    }

    Ok(WorkflowRuntimeTreeResponse {
        total_workflows: page.total.max(0) as usize,
        pagination: WorkflowRuntimeTreePagination::new(
            page.limit,
            page.offset,
            by_id.len(),
            page.total,
            job_limit,
            mode.command_limit(),
            mode.detail(),
            false,
        ),
        summary,
        workflows,
    })
}

async fn workflow_runtime_tree_summary(
    store: &harness_workflow::runtime::WorkflowRuntimeStore,
    project_id: Option<&str>,
) -> anyhow::Result<WorkflowRuntimeTreeSummary> {
    let aggregate_summary = store
        .runtime_summary_counts_for_instances(
            project_id,
            ACTIVITY_RESULT_ENVELOPE_ARTIFACT_TYPE,
            ACTIVITY_RESULT_ENVELOPE_SCHEMA,
        )
        .await?;
    Ok(WorkflowRuntimeTreeSummary {
        total_commands: aggregate_summary.total_commands,
        total_runtime_jobs: aggregate_summary.total_runtime_jobs,
        command_statuses: aggregate_summary.command_statuses,
        runtime_job_statuses: aggregate_summary.runtime_job_statuses,
        running_job_lease_statuses: aggregate_summary.running_job_lease_statuses,
        activity_outcomes: aggregate_summary.activity_outcomes,
        jobs_without_activity_envelope: aggregate_summary.jobs_without_activity_envelope,
    })
}

async fn build_full_workflow_runtime_nodes(
    store: &harness_workflow::runtime::WorkflowRuntimeStore,
    instances: Vec<WorkflowInstance>,
    workflow_ids: &[String],
    job_limit: usize,
) -> anyhow::Result<(
    BTreeMap<String, WorkflowRuntimeTreeNode>,
    BTreeMap<String, Vec<String>>,
)> {
    let mut events_by_workflow = store.events_for_workflows(workflow_ids).await?;
    let mut decisions_by_workflow = store.decisions_for_workflows(workflow_ids).await?;
    let mut commands_by_workflow = store.commands_for_workflows(workflow_ids).await?;
    let command_ids: Vec<String> = commands_by_workflow
        .values()
        .flat_map(|commands| commands.iter().map(|command| command.id.clone()))
        .collect();
    let runtime_job_counts_by_command = store.runtime_job_counts_for_commands(&command_ids).await?;
    let mut runtime_jobs_by_command = store
        .runtime_jobs_for_commands_limited(&command_ids, job_limit as i64)
        .await?;
    let runtime_job_ids: Vec<String> = runtime_jobs_by_command
        .values()
        .flat_map(|jobs| jobs.iter().map(|job| job.id.clone()))
        .collect();
    let mut runtime_events_by_job = store.runtime_events_for_jobs(&runtime_job_ids).await?;

    let mut by_id = BTreeMap::new();
    let mut children_by_parent: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for instance in instances {
        let workflow_id = instance.id.clone();
        if let Some(parent_id) = instance.parent_workflow_id.as_ref() {
            children_by_parent
                .entry(parent_id.clone())
                .or_default()
                .push(workflow_id.clone());
        }
        let events = events_by_workflow.remove(&workflow_id).unwrap_or_default();
        let decisions = decisions_by_workflow
            .remove(&workflow_id)
            .unwrap_or_default();
        let rejected_decision_count = decisions
            .iter()
            .filter(|decision| !decision.accepted)
            .count();
        let command_records = commands_by_workflow
            .remove(&workflow_id)
            .unwrap_or_default();
        let command_count = command_records.len();
        let commands: Vec<WorkflowRuntimeCommandNode> = command_records
            .into_iter()
            .map(|command| {
                let runtime_job_count = runtime_job_counts_by_command
                    .get(&command.id)
                    .copied()
                    .unwrap_or_default();
                let runtime_jobs = runtime_jobs_by_command
                    .remove(&command.id)
                    .unwrap_or_default()
                    .into_iter()
                    .map(|job| {
                        let runtime_events =
                            runtime_events_by_job.remove(&job.id).unwrap_or_default();
                        WorkflowRuntimeJobNode::full(job, runtime_events)
                    })
                    .collect();
                WorkflowRuntimeCommandNode::new(command, runtime_job_count, runtime_jobs)
            })
            .collect();
        let runtime_job_count = commands
            .iter()
            .map(|command| command.runtime_job_count)
            .sum();
        by_id.insert(
            workflow_id,
            WorkflowRuntimeTreeNode {
                workflow: instance,
                runtime_job_count,
                event_count: events.len(),
                decision_count: decisions.len(),
                rejected_decision_count,
                command_count,
                events,
                decisions,
                commands,
                children: Vec::new(),
            },
        );
    }
    Ok((by_id, children_by_parent))
}

async fn build_compact_workflow_runtime_nodes(
    store: &harness_workflow::runtime::WorkflowRuntimeStore,
    instances: Vec<WorkflowInstance>,
    workflow_ids: &[String],
    job_limit: usize,
    command_limit: usize,
    rejected_decision_limit: usize,
) -> anyhow::Result<(
    BTreeMap<String, WorkflowRuntimeTreeNode>,
    BTreeMap<String, Vec<String>>,
)> {
    let detail_counts_by_workflow = store.detail_counts_for_workflows(workflow_ids).await?;
    let mut decisions_by_workflow = store
        .rejected_decisions_for_workflows_limited(workflow_ids, rejected_decision_limit as i64)
        .await?;
    let mut commands_by_workflow = store
        .commands_for_workflows_limited(workflow_ids, command_limit as i64)
        .await?;
    let command_ids: Vec<String> = commands_by_workflow
        .values()
        .flat_map(|commands| commands.iter().map(|command| command.id.clone()))
        .collect();
    let runtime_job_counts_by_command = store.runtime_job_counts_for_commands(&command_ids).await?;
    let mut runtime_jobs_by_command = store
        .compact_runtime_jobs_for_commands_limited(&command_ids, job_limit as i64)
        .await?;
    let runtime_job_ids: Vec<String> = runtime_jobs_by_command
        .values()
        .flat_map(|jobs| jobs.iter().map(|job| job.id.clone()))
        .collect();
    let mut runtime_event_summaries_by_job = store
        .runtime_event_summaries_for_jobs(&runtime_job_ids)
        .await?;

    let mut by_id = BTreeMap::new();
    let mut children_by_parent: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for instance in instances {
        let workflow_id = instance.id.clone();
        if let Some(parent_id) = instance.parent_workflow_id.as_ref() {
            children_by_parent
                .entry(parent_id.clone())
                .or_default()
                .push(workflow_id.clone());
        }
        let detail_counts = detail_counts_by_workflow
            .get(&workflow_id)
            .cloned()
            .unwrap_or_default();
        let decisions = decisions_by_workflow
            .remove(&workflow_id)
            .unwrap_or_default();
        let commands: Vec<WorkflowRuntimeCommandNode> = commands_by_workflow
            .remove(&workflow_id)
            .unwrap_or_default()
            .into_iter()
            .map(|mut command| {
                command.command = compact_workflow_command(command.command);
                let runtime_job_count = runtime_job_counts_by_command
                    .get(&command.id)
                    .copied()
                    .unwrap_or_default();
                let runtime_jobs = runtime_jobs_by_command
                    .remove(&command.id)
                    .unwrap_or_default()
                    .into_iter()
                    .map(|job| {
                        let runtime_summary = runtime_event_summaries_by_job
                            .remove(&job.id)
                            .unwrap_or_else(|| RuntimeEventSummary {
                                runtime_job_id: job.id.clone(),
                                ..Default::default()
                            });
                        WorkflowRuntimeJobNode::compact(job, runtime_summary)
                    })
                    .collect();
                WorkflowRuntimeCommandNode::new(command, runtime_job_count, runtime_jobs)
            })
            .collect();
        by_id.insert(
            workflow_id,
            WorkflowRuntimeTreeNode {
                workflow: instance,
                runtime_job_count: detail_counts.runtime_job_count,
                event_count: detail_counts.event_count,
                decision_count: detail_counts.decision_count,
                rejected_decision_count: detail_counts.rejected_decision_count,
                command_count: detail_counts.command_count,
                events: Vec::new(),
                decisions,
                commands,
                children: Vec::new(),
            },
        );
    }
    Ok((by_id, children_by_parent))
}

fn compact_workflow_command(command: WorkflowCommand) -> WorkflowCommand {
    let WorkflowCommand {
        command_type,
        dedupe_key,
        command,
    } = command;
    let Value::Object(object) = command else {
        return WorkflowCommand {
            command_type,
            dedupe_key,
            command,
        };
    };

    let mut payload = serde_json::Map::new();
    for key in [
        "activity",
        "definition_id",
        "subject_key",
        "pr_number",
        "pr_url",
        "reason",
        "concern",
    ] {
        if let Some(value) = object.get(key) {
            payload.insert(key.to_string(), value.clone());
        }
    }
    WorkflowCommand {
        command_type,
        dedupe_key,
        command: Value::Object(payload),
    }
}

#[cfg(test)]
fn apply_runtime_activity_summary(
    summary: &mut WorkflowRuntimeTreeSummary,
    jobs_by_command: &BTreeMap<String, Vec<RuntimeJob>>,
) {
    for job in jobs_by_command.values().flatten() {
        if let Some(outcome) = activity_result_envelope_from_job(job).and_then(|envelope| {
            envelope
                .get("outcome")
                .and_then(Value::as_str)
                .map(str::to_string)
        }) {
            summary
                .activity_outcomes
                .entry(outcome)
                .and_modify(|count| *count += 1)
                .or_insert(1);
        } else {
            summary.jobs_without_activity_envelope += 1;
        }
    }
}

fn prompt_packet_digest_from_events(events: &[RuntimeEvent]) -> Option<String> {
    events.iter().rev().find_map(|event| {
        if event.event_type != "RuntimePromptPrepared" {
            return None;
        }
        event
            .event
            .get("prompt_packet_digest")
            .and_then(Value::as_str)
            .map(str::to_string)
    })
}

fn runtime_job_has_in_flight_model_turn(job: &RuntimeJob, events: &[RuntimeEvent]) -> bool {
    if job.status != RuntimeJobStatus::Running {
        return false;
    }
    let Some(latest_turn_sequence) = events
        .iter()
        .filter(|event| event.event_type == "RuntimeTurnStarted")
        .map(|event| event.sequence)
        .max()
    else {
        return false;
    };
    !events.iter().any(|event| {
        event.event_type == "ActivityResultReady" && event.sequence > latest_turn_sequence
    })
}

fn last_runtime_observation_at(
    job: &RuntimeJob,
    events: &[RuntimeEvent],
) -> Option<chrono::DateTime<chrono::Utc>> {
    let latest_event_at = events.last().map(|event| event.created_at);
    if job.status == RuntimeJobStatus::Running {
        return Some(match latest_event_at {
            Some(created_at) => created_at.max(job.updated_at),
            None => job.updated_at,
        });
    }
    latest_event_at
}

fn activity_result_envelope_from_job(job: &RuntimeJob) -> Option<Value> {
    let artifacts = job.output.as_ref()?.get("artifacts")?.as_array()?;
    artifacts.iter().rev().find_map(|artifact| {
        if artifact.get("artifact_type").and_then(Value::as_str)
            != Some(ACTIVITY_RESULT_ENVELOPE_ARTIFACT_TYPE)
        {
            return None;
        }
        let payload = artifact.get("artifact")?;
        if payload.get("schema").and_then(Value::as_str) != Some(ACTIVITY_RESULT_ENVELOPE_SCHEMA) {
            return None;
        }
        Some(payload.clone())
    })
}

fn attach_workflow_children(
    workflow_id: &str,
    by_id: &BTreeMap<String, WorkflowRuntimeTreeNode>,
    children_by_parent: &BTreeMap<String, Vec<String>>,
    path: &mut BTreeSet<String>,
) -> Option<WorkflowRuntimeTreeNode> {
    if !path.insert(workflow_id.to_string()) {
        return None;
    }
    let mut node = by_id.get(workflow_id)?.clone();
    node.children = children_by_parent
        .get(workflow_id)
        .into_iter()
        .flatten()
        .filter_map(|child_id| attach_workflow_children(child_id, by_id, children_by_parent, path))
        .collect();
    path.remove(workflow_id);
    Some(node)
}

#[cfg(test)]
#[path = "misc_routes_runtime_tree_tests.rs"]
mod runtime_tree_tests;

pub(crate) async fn handle_rpc(
    State(state): State<Arc<AppState>>,
    Json(req): Json<RpcRequest>,
) -> Response {
    match router::handle_request(&state, req).await {
        Some(resp) => (StatusCode::OK, Json(resp)).into_response(),
        None => StatusCode::NO_CONTENT.into_response(),
    }
}

fn configured_github_webhook_project_root(
    github: Option<&harness_core::config::intake::GitHubIntakeConfig>,
    default_root: &StdPath,
    repo: &str,
) -> Option<PathBuf> {
    github?
        .effective_repos()
        .into_iter()
        .find(|repo_cfg| repo_cfg.repo == repo)
        .map(|repo_cfg| {
            repo_cfg
                .project_root
                .map(PathBuf::from)
                .unwrap_or_else(|| default_root.to_path_buf())
        })
}

enum GitHubWebhookProjectRootError {
    RepoNotConfigured(String),
    RegistryLookup(String),
}

fn github_webhook_project_root_error_response(
    error: GitHubWebhookProjectRootError,
) -> (StatusCode, Json<serde_json::Value>) {
    match error {
        // Treat unknown repositories as ignored so GitHub does not retry
        // an event for a repo this harness instance is not configured to
        // serve. Registry failures remain internal errors.
        GitHubWebhookProjectRootError::RepoNotConfigured(reason) => (
            StatusCode::OK,
            Json(json!({ "status": "ignored", "reason": reason })),
        ),
        GitHubWebhookProjectRootError::RegistryLookup(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": error })),
        ),
    }
}

async fn resolve_github_webhook_project_root(
    state: &Arc<AppState>,
    repo: &str,
) -> Result<PathBuf, GitHubWebhookProjectRootError> {
    if let Some(project_root) = configured_github_webhook_project_root(
        state.core.server.config.intake.github.as_ref(),
        &state.core.project_root,
        repo,
    ) {
        return Ok(project_root);
    }

    if let Some(registry) = state.core.project_registry.as_deref() {
        if let Some(project) = registry.get(repo).await.map_err(|error| {
            GitHubWebhookProjectRootError::RegistryLookup(format!(
                "project registry lookup failed: {error}"
            ))
        })? {
            return Ok(project.root);
        }
        if let Some(project) = registry.get_by_name(repo).await.map_err(|error| {
            GitHubWebhookProjectRootError::RegistryLookup(format!(
                "project registry lookup failed: {error}"
            ))
        })? {
            return Ok(project.root);
        }
    }

    Err(GitHubWebhookProjectRootError::RepoNotConfigured(format!(
        "webhook repository '{repo}' is not configured in intake.github and was not found in the project registry"
    )))
}

#[cfg(test)]
mod tests {
    use super::{
        configured_github_webhook_project_root, github_webhook_project_root_error_response,
        GitHubWebhookProjectRootError,
    };
    use axum::http::StatusCode;
    use harness_core::config::intake::{GitHubIntakeConfig, GitHubRepoConfig};
    use std::path::PathBuf;

    #[test]
    fn multi_repo_github_webhook_uses_repo_specific_project_root_override() {
        let default_root = PathBuf::from("/srv/repo-a");
        let github = GitHubIntakeConfig {
            enabled: true,
            repos: vec![
                GitHubRepoConfig {
                    repo: "org/repo-a".to_string(),
                    label: "harness".to_string(),
                    project_root: None,
                },
                GitHubRepoConfig {
                    repo: "org/repo-b".to_string(),
                    label: "harness".to_string(),
                    project_root: Some("/srv/repo-b".to_string()),
                },
            ],
            ..Default::default()
        };

        let resolved =
            configured_github_webhook_project_root(Some(&github), &default_root, "org/repo-b");

        assert_eq!(resolved, Some(PathBuf::from("/srv/repo-b")));
    }

    #[test]
    fn configured_github_repo_without_override_falls_back_to_default_project_root() {
        let default_root = PathBuf::from("/srv/repo-a");
        let github = GitHubIntakeConfig {
            enabled: true,
            repos: vec![GitHubRepoConfig {
                repo: "org/repo-a".to_string(),
                label: "harness".to_string(),
                project_root: None,
            }],
            ..Default::default()
        };

        let resolved =
            configured_github_webhook_project_root(Some(&github), &default_root, "org/repo-a");

        assert_eq!(resolved, Some(default_root));
    }

    #[test]
    fn unconfigured_github_repo_has_no_configured_project_root() {
        let default_root = PathBuf::from("/srv/repo-a");
        let github = GitHubIntakeConfig {
            enabled: true,
            repos: vec![GitHubRepoConfig {
                repo: "org/repo-a".to_string(),
                label: "harness".to_string(),
                project_root: None,
            }],
            ..Default::default()
        };

        let resolved =
            configured_github_webhook_project_root(Some(&github), &default_root, "org/repo-b");

        assert_eq!(resolved, None);
    }

    #[test]
    fn unconfigured_github_repo_returns_ignored_response() {
        let (status, body) = github_webhook_project_root_error_response(
            GitHubWebhookProjectRootError::RepoNotConfigured(
                "webhook repository 'org/repo-b' is not configured".to_string(),
            ),
        );

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body.0["status"], "ignored");
        assert!(body.0["reason"]
            .as_str()
            .unwrap_or_default()
            .contains("not configured"));
    }

    #[test]
    fn registry_lookup_failures_return_internal_server_error() {
        let (status, body) = github_webhook_project_root_error_response(
            GitHubWebhookProjectRootError::RegistryLookup(
                "project registry lookup failed: boom".to_string(),
            ),
        );

        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(body.0["error"], "project registry lookup failed: boom");
    }
}

pub(crate) async fn github_webhook(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> (StatusCode, Json<serde_json::Value>) {
    let secret = match state
        .core
        .server
        .config
        .server
        .github_webhook_secret
        .as_deref()
    {
        Some("") => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "invalid server.github_webhook_secret configuration"})),
            )
        }
        Some(secret) => secret,
        None => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "missing server.github_webhook_secret configuration"})),
            )
        }
    };
    let signature = match headers
        .get("x-hub-signature-256")
        .and_then(|value| value.to_str().ok())
    {
        Some(signature) => signature,
        None => {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({"error": "missing header x-hub-signature-256"})),
            )
        }
    };
    if !crate::webhook::verify_github_signature(secret, signature, body.as_ref()) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "invalid webhook signature"})),
        );
    }

    let event = match headers
        .get("x-github-event")
        .and_then(|value| value.to_str().ok())
    {
        Some(event) => event,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "missing header x-github-event"})),
            )
        }
    };
    if !crate::webhook::is_valid_github_event_name(event) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "invalid header x-github-event"})),
        );
    }

    // Autonomous webhook intake requires github intake to be enabled AND the
    // mode to opt in (webhook/hybrid). Honor the per-repo label filter so the
    // webhook only auto-enqueues issues the poller would have considered.
    let github = state.core.server.config.intake.github.as_ref();
    let autonomous_issues = github
        .map(|github| github.enabled && github.mode.webhook_autonomous())
        .unwrap_or(false);
    let autonomous_label = github.and_then(|github| {
        let repo = serde_json::from_slice::<serde_json::Value>(body.as_ref())
            .ok()
            .and_then(|value| {
                value
                    .get("repository")
                    .and_then(|repo| repo.get("full_name"))
                    .and_then(|name| name.as_str())
                    .map(str::to_string)
            });
        repo.and_then(|repo| github.find_repo_config(&repo))
            .map(|cfg| cfg.label)
            .or_else(|| Some(github.label.clone()))
    });
    let (request, reason) = match crate::webhook::parse_github_webhook_task_request(
        event,
        body.as_ref(),
        autonomous_issues,
        autonomous_label.as_deref(),
    ) {
        Ok(parsed) => parsed,
        Err(error) => return (StatusCode::BAD_REQUEST, Json(json!({ "error": error }))),
    };

    let Some(mut req) = request else {
        return (
            StatusCode::OK,
            Json(json!({
                "status": "ignored",
                "reason": reason,
            })),
        );
    };

    if req.project.is_none() {
        req.project = Some(match req.repo.as_deref() {
            Some(repo) => match resolve_github_webhook_project_root(&state, repo).await {
                Ok(project_root) => project_root,
                Err(error) => return github_webhook_project_root_error_response(error),
            },
            None => state.core.project_root.clone(),
        });
    }

    let is_issue_submission = req.issue.is_some();
    match task_routes::enqueue_task(&state, req).await {
        Ok(task_id) => {
            match task_routes::task_response_details(&state, &task_id, is_issue_submission).await {
                Ok(details) => (
                    StatusCode::ACCEPTED,
                    Json(json!({
                        "status": if details.execution_path == "workflow_runtime" {
                            details.status
                        } else {
                            "accepted".to_string()
                        },
                        "reason": reason,
                        "task_id": task_id.0,
                        "execution_path": details.execution_path,
                    })),
                ),
                Err(crate::services::execution::EnqueueTaskError::BadRequest(error)) => {
                    (StatusCode::BAD_REQUEST, Json(json!({ "error": error })))
                }
                Err(crate::services::execution::EnqueueTaskError::Internal(error)) => (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({ "error": error })),
                ),
                Err(crate::services::execution::EnqueueTaskError::MaintenanceWindow {
                    retry_after_secs,
                }) => (
                    StatusCode::SERVICE_UNAVAILABLE,
                    Json(json!({ "error": "maintenance_window", "retry_after": retry_after_secs })),
                ),
            }
        }
        Err(crate::services::execution::EnqueueTaskError::BadRequest(error)) => {
            (StatusCode::BAD_REQUEST, Json(json!({ "error": error })))
        }
        Err(crate::services::execution::EnqueueTaskError::Internal(error)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": error })),
        ),
        Err(crate::services::execution::EnqueueTaskError::MaintenanceWindow {
            retry_after_secs,
        }) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "error": "maintenance_window", "retry_after": retry_after_secs })),
        ),
    }
}

/// GET /api/intake — current status of all intake channels and recent dispatches.
pub(crate) async fn intake_status(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let intake_config = &state.core.server.config.intake;
    let all_tasks = state.core.tasks.list_all();
    let (runtime_issue_workflows, runtime_degraded) =
        runtime_issue_workflows_for_intake_status(&state).await;

    let github_active: u64 = if let Some(store) = state.core.issue_workflow_store.as_ref() {
        match store.list().await {
            Ok(workflows) => workflows
                .into_iter()
                .filter(|workflow| {
                    !matches!(
                        workflow.state,
                        harness_workflow::issue_lifecycle::IssueLifecycleState::Done
                            | harness_workflow::issue_lifecycle::IssueLifecycleState::Failed
                            | harness_workflow::issue_lifecycle::IssueLifecycleState::Cancelled
                    )
                })
                .count() as u64,
            Err(_) => all_tasks
                .iter()
                .filter(|t| {
                    t.source.as_deref() == Some("github")
                        && !matches!(
                            t.status,
                            task_runner::TaskStatus::Done | task_runner::TaskStatus::Failed
                        )
                })
                .count() as u64,
        }
    } else {
        all_tasks
            .iter()
            .filter(|t| {
                t.source.as_deref() == Some("github")
                    && !matches!(
                        t.status,
                        task_runner::TaskStatus::Done | task_runner::TaskStatus::Failed
                    )
            })
            .count() as u64
    } + runtime_issue_workflows
        .iter()
        .filter(|workflow| runtime_workflow_has_tracker_source(workflow, "github"))
        .filter(|workflow| !workflow.is_terminal())
        .count() as u64;

    let feishu_active: u64 = all_tasks
        .iter()
        .filter(|t| {
            t.source.as_deref() == Some("feishu")
                && !matches!(
                    t.status,
                    task_runner::TaskStatus::Done | task_runner::TaskStatus::Failed
                )
        })
        .count() as u64;

    let dashboard_active: u64 = all_tasks
        .iter()
        .filter(|t| {
            (t.source.as_deref() == Some("dashboard") || t.source.is_none())
                && !matches!(
                    t.status,
                    task_runner::TaskStatus::Done | task_runner::TaskStatus::Failed
                )
        })
        .count() as u64;

    let github_channel = json!({
        "name": "github",
        "enabled": intake_config.github.as_ref().map(|c| c.enabled).unwrap_or(false),
        "repo": intake_config.github.as_ref().map(|c| c.repo.as_str()).unwrap_or(""),
        "active": github_active,
    });

    let feishu_channel = json!({
        "name": "feishu",
        "enabled": state.intake.feishu_intake.is_some(),
        "keyword": intake_config.feishu.as_ref().map(|c| c.trigger_keyword.as_str()).unwrap_or(""),
        "active": feishu_active,
    });

    let dashboard_channel = json!({
        "name": "dashboard",
        "enabled": true,
        "active": dashboard_active,
    });

    let mut recent_dispatches: Vec<IntakeRecentDispatch> = all_tasks
        .iter()
        .filter(|t| t.source.is_some())
        .map(legacy_task_recent_dispatch)
        .collect();
    recent_dispatches.extend(
        runtime_issue_workflows
            .iter()
            .filter(|workflow| runtime_workflow_intake_source(workflow).is_some())
            .filter_map(runtime_issue_recent_dispatch),
    );
    recent_dispatches.sort_by(|left, right| right.sort_at.cmp(&left.sort_at));
    let recent_dispatches: Vec<serde_json::Value> = recent_dispatches
        .into_iter()
        .take(10)
        .map(|dispatch| dispatch.payload)
        .collect();

    let mut response = json!({
        "channels": [github_channel, feishu_channel, dashboard_channel],
        "recent_dispatches": recent_dispatches,
    });
    if runtime_degraded {
        response["degraded"] = json!({
            "partial": true,
            "missing": ["workflow_runtime_submissions"],
            "reason": "runtime_submission_summaries_unavailable",
        });
    }
    Json(response)
}

async fn runtime_issue_workflows_for_intake_status(
    state: &AppState,
) -> (Vec<WorkflowInstance>, bool) {
    let Some(store) = state.core.workflow_runtime_store.as_ref() else {
        return (
            Vec::new(),
            workflow_runtime_submissions_expected_but_unavailable(state),
        );
    };
    match store
        .list_instances_by_definition(
            harness_workflow::runtime::GITHUB_ISSUE_PR_DEFINITION_ID,
            None,
            None,
        )
        .await
    {
        Ok(workflows) => (workflows, false),
        Err(error) => {
            tracing::error!("intake_status: workflow runtime lookup failed: {error}");
            (Vec::new(), true)
        }
    }
}

fn legacy_task_recent_dispatch(task: &task_runner::TaskState) -> IntakeRecentDispatch {
    IntakeRecentDispatch {
        sort_at: parse_rfc3339_utc(task.created_at.as_deref()),
        payload: json!({
            "source": task.source,
            "external_id": task.external_id,
            "task_id": task.id.0,
            "status": serde_json::to_value(&task.status).unwrap_or(json!("unknown")),
            "pr_url": task.pr_url,
        }),
    }
}

fn runtime_issue_recent_dispatch(workflow: &WorkflowInstance) -> Option<IntakeRecentDispatch> {
    let projection = RuntimeWorkflowProjection::from_workflow(workflow);
    let task_id = projection.submission_handle?;
    let source = runtime_workflow_intake_source(workflow)?;
    Some(IntakeRecentDispatch {
        sort_at: Some(workflow.created_at),
        payload: json!({
            "source": source,
            "external_id": runtime_workflow_external_id(workflow),
            "tracker_source": runtime_workflow_tracker_source(workflow),
            "tracker_external_id": runtime_workflow_tracker_external_id(workflow),
            "task_id": task_id.0,
            "status": serde_json::to_value(&projection.task_status).unwrap_or(json!("unknown")),
            "pr_url": runtime_workflow_data_string(workflow, "pr_url"),
        }),
    })
}

fn parse_rfc3339_utc(value: Option<&str>) -> Option<chrono::DateTime<chrono::Utc>> {
    value
        .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
        .map(|value| value.with_timezone(&chrono::Utc))
}

fn workflow_runtime_submissions_expected_but_unavailable(state: &AppState) -> bool {
    state
        .degraded_subsystems
        .contains(&"workflow_runtime_store")
        || state
            .startup_statuses
            .iter()
            .any(|status| status.name == "workflow_runtime_store" && !status.ready)
}

fn runtime_workflow_has_tracker_source(workflow: &WorkflowInstance, source: &str) -> bool {
    runtime_workflow_tracker_source(workflow)
        .or_else(|| runtime_workflow_data_string(workflow, "source"))
        .as_deref()
        == Some(source)
}

fn runtime_workflow_intake_source(workflow: &WorkflowInstance) -> Option<String> {
    runtime_workflow_data_string(workflow, "source")
        .or_else(|| runtime_workflow_tracker_source(workflow))
}

fn runtime_workflow_external_id(workflow: &WorkflowInstance) -> Option<String> {
    runtime_workflow_data_string(workflow, "external_id")
        .or_else(|| runtime_workflow_tracker_external_id(workflow))
}

fn runtime_workflow_tracker_source(workflow: &WorkflowInstance) -> Option<String> {
    runtime_workflow_data_string(workflow, "tracker_source")
}

fn runtime_workflow_tracker_external_id(workflow: &WorkflowInstance) -> Option<String> {
    runtime_workflow_data_string(workflow, "tracker_external_id")
}

fn runtime_workflow_data_string(workflow: &WorkflowInstance, field: &str) -> Option<String> {
    workflow
        .data
        .get(field)
        .and_then(serde_json::Value::as_str)
        .map(ToOwned::to_owned)
}

#[derive(serde::Deserialize)]
pub(crate) struct IngestSignalRequest {
    pub(crate) source: String,
    #[serde(default)]
    pub(crate) severity: Option<harness_core::types::Severity>,
    pub(crate) payload: serde_json::Value,
}

/// Infer severity from a GitHub webhook payload: CI failure → High, changes_requested → Medium.
pub(crate) fn infer_github_severity(
    payload: &serde_json::Value,
) -> Option<harness_core::types::Severity> {
    if let Some(obj) = payload.as_object() {
        if let (Some(action), Some(check_run)) = (
            obj.get("action").and_then(|v| v.as_str()),
            obj.get("check_run"),
        ) {
            if action == "completed" {
                let conclusion = check_run
                    .get("conclusion")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if conclusion == "failure" {
                    return Some(harness_core::types::Severity::High);
                }
            }
        }
        if let Some(review) = obj.get("review") {
            let state = review.get("state").and_then(|v| v.as_str()).unwrap_or("");
            if state.eq_ignore_ascii_case("changes_requested") {
                return Some(harness_core::types::Severity::Medium);
            }
        }
    }
    None
}

/// POST /signals — ingest an external signal (CI failure, review feedback, etc.).
///
/// Validates the `x-hub-signature-256` HMAC-SHA256 header using the configured
/// `server.github_webhook_secret`. Rate-limited to 100 requests per source per minute.
pub(crate) async fn ingest_signal(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> (StatusCode, Json<serde_json::Value>) {
    let secret = match state
        .core
        .server
        .config
        .server
        .github_webhook_secret
        .as_deref()
    {
        Some("") | None => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "server.github_webhook_secret not configured"})),
            )
        }
        Some(s) => s,
    };

    let signature = match headers
        .get("x-hub-signature-256")
        .and_then(|v| v.to_str().ok())
    {
        Some(sig) => sig,
        None => {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({"error": "missing header x-hub-signature-256"})),
            )
        }
    };

    if !crate::webhook::verify_github_signature(secret, signature, body.as_ref()) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "invalid webhook signature"})),
        );
    }

    let req: IngestSignalRequest = match serde_json::from_slice(&body) {
        Ok(r) => r,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": format!("invalid payload: {e}")})),
            )
        }
    };

    if !state
        .observability
        .signal_rate_limiter
        .check_and_increment(&req.source)
    {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            Json(json!({"error": "rate limit exceeded: max 100 signals per minute per source"})),
        );
    }

    let severity = req.severity.unwrap_or_else(|| {
        if req.source == "github" {
            infer_github_severity(&req.payload).unwrap_or(harness_core::types::Severity::Low)
        } else {
            harness_core::types::Severity::Low
        }
    });

    let signal =
        harness_core::types::ExternalSignal::new(req.source.clone(), severity, req.payload.clone());
    let signal_id = signal.id.clone();

    if let Err(e) = state.observability.events.log_external_signal(&signal) {
        tracing::error!(source = %req.source, "failed to store external signal: {e}");
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": "failed to store signal"})),
        );
    }

    tracing::info!(
        source = %req.source,
        severity = ?severity,
        signal_id = %signal_id,
        "external signal ingested"
    );

    (
        StatusCode::OK,
        Json(json!({"status": "accepted", "id": signal_id.as_str()})),
    )
}

#[derive(serde::Deserialize)]
pub(crate) struct PasswordResetRequest {
    pub(crate) email: String,
}

pub(crate) fn prepare_password_reset_request(
    rate_limiter: &crate::http::rate_limit::PasswordResetRateLimiter,
    limit: u32,
    email: &str,
) -> Result<String, (StatusCode, serde_json::Value)> {
    let email = email.trim().to_lowercase();
    if email.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            json!({"error": "email is required"}),
        ));
    }

    if !rate_limiter.check_and_increment(&email) {
        return Err((
            StatusCode::TOO_MANY_REQUESTS,
            json!({
                "error": format!(
                    "rate limit exceeded: max {} password reset requests per hour",
                    limit
                )
            }),
        ));
    }

    Ok(email)
}

pub(crate) fn disabled_password_reset_response() -> (StatusCode, serde_json::Value) {
    (
        StatusCode::NOT_IMPLEMENTED,
        json!({"error": "password reset is not yet implemented"}),
    )
}

/// POST /auth/reset-password — temporarily disabled until email delivery exists.
///
/// Requests are still validated, rate-limited, and logged so the auth-exempt
/// endpoint retains its abuse protections while the actual reset flow is
/// unavailable.
pub(crate) async fn password_reset(
    State(state): State<Arc<AppState>>,
    Json(req): Json<PasswordResetRequest>,
) -> (StatusCode, Json<serde_json::Value>) {
    let limit = state
        .core
        .server
        .config
        .server
        .password_reset_rate_limit_per_hour;
    let email = match prepare_password_reset_request(
        &state.observability.password_reset_rate_limiter,
        limit,
        &req.email,
    ) {
        Ok(email) => email,
        Err((status, body)) => return (status, Json(body)),
    };

    tracing::info!(
        email_hash = %format!("{:x}", {
            use std::hash::{Hash, Hasher};
            let mut h = std::collections::hash_map::DefaultHasher::new();
            email.hash(&mut h);
            h.finish()
        }),
        "password reset requested while endpoint disabled"
    );

    // TODO: wire up SMTP/transactional email before enabling this endpoint.
    let (status, body) = disabled_password_reset_response();
    (status, Json(body))
}
