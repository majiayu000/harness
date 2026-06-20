use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;
#[cfg(test)]
use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::Arc;

use crate::http::state::AppState;
use crate::runtime_projection::{RuntimeActiveBucket, RuntimeWorkflowProjection};
#[path = "misc_routes_runtime_tree_nodes.rs"]
mod nodes;
#[cfg(test)]
use harness_workflow::runtime::RuntimeJob;
#[cfg(test)]
use nodes::{activity_result_envelope_from_job, runtime_job_has_in_flight_model_turn};
use nodes::{
    compact_workflow_command, WorkflowRuntimeCommandNode, WorkflowRuntimeJobNode,
    WorkflowRuntimeTreeNode, WorkflowRuntimeTreeProjection, ACTIVITY_RESULT_ENVELOPE_ARTIFACT_TYPE,
    ACTIVITY_RESULT_ENVELOPE_SCHEMA,
};

use harness_workflow::runtime::store::RuntimeEventSummary;
use harness_workflow::runtime::{WorkflowInstance, WorkflowSubject};

const WORKFLOW_RUNTIME_TREE_DEFAULT_LIMIT: i64 = 100;
const WORKFLOW_RUNTIME_TREE_MAX_LIMIT: i64 = 500;
const WORKFLOW_RUNTIME_TREE_DEFAULT_JOB_LIMIT: i64 = 5;
const WORKFLOW_RUNTIME_TREE_MAX_JOB_LIMIT: i64 = 50;
const WORKFLOW_RUNTIME_TREE_DEFAULT_COMMAND_LIMIT: i64 = 5;
const WORKFLOW_RUNTIME_TREE_MAX_COMMAND_LIMIT: i64 = 50;
const WORKFLOW_RUNTIME_TREE_DEFAULT_REJECTED_DECISION_LIMIT: i64 = 3;
const WORKFLOW_RUNTIME_TREE_MAX_REJECTED_DECISION_LIMIT: i64 = 20;

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
    pub workflow_statuses: BTreeMap<String, usize>,
    pub workflow_scheduler_states: BTreeMap<String, usize>,
    pub workflow_active_buckets: BTreeMap<String, usize>,
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

    let (mut by_id, children_by_parent) = match mode {
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

    let returned = by_id.len();
    let mut workflows = Vec::new();
    for root_id in root_ids {
        if let Some(node) = attach_workflow_children(&root_id, &mut by_id, &children_by_parent) {
            workflows.push(node);
        }
    }

    Ok(WorkflowRuntimeTreeResponse {
        total_workflows: page.total.max(0) as usize,
        pagination: WorkflowRuntimeTreePagination::new(
            page.limit,
            page.offset,
            returned,
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
    let (workflow_statuses, workflow_scheduler_states, workflow_active_buckets) =
        workflow_projection_summary_counts(&aggregate_summary.workflow_states);
    Ok(WorkflowRuntimeTreeSummary {
        workflow_statuses,
        workflow_scheduler_states,
        workflow_active_buckets,
        total_commands: aggregate_summary.total_commands,
        total_runtime_jobs: aggregate_summary.total_runtime_jobs,
        command_statuses: aggregate_summary.command_statuses,
        runtime_job_statuses: aggregate_summary.runtime_job_statuses,
        running_job_lease_statuses: aggregate_summary.running_job_lease_statuses,
        activity_outcomes: aggregate_summary.activity_outcomes,
        jobs_without_activity_envelope: aggregate_summary.jobs_without_activity_envelope,
    })
}

fn workflow_projection_summary_counts(
    workflow_states: &[harness_workflow::runtime::store::WorkflowRuntimeStateCount],
) -> (
    BTreeMap<String, usize>,
    BTreeMap<String, usize>,
    BTreeMap<String, usize>,
) {
    let mut workflow_statuses = BTreeMap::new();
    let mut workflow_scheduler_states = BTreeMap::new();
    let mut workflow_active_buckets = BTreeMap::new();

    for state_count in workflow_states {
        let workflow = WorkflowInstance::new(
            &state_count.definition_id,
            1,
            &state_count.state,
            WorkflowSubject::new(
                "summary",
                format!("{}:{}", state_count.definition_id, state_count.state),
            ),
        );
        let projection = RuntimeWorkflowProjection::from_workflow(&workflow);
        add_summary_count(
            &mut workflow_statuses,
            projection.task_status.as_str(),
            state_count.count,
        );
        add_summary_count(
            &mut workflow_scheduler_states,
            projection.scheduler.authority_state.as_str(),
            state_count.count,
        );
        if let Some(active_bucket) = projection.active_bucket() {
            add_summary_count(
                &mut workflow_active_buckets,
                runtime_active_bucket_label(active_bucket),
                state_count.count,
            );
        }
    }

    (
        workflow_statuses,
        workflow_scheduler_states,
        workflow_active_buckets,
    )
}

fn add_summary_count(counts: &mut BTreeMap<String, usize>, key: &str, count: usize) {
    counts
        .entry(key.to_string())
        .and_modify(|current| *current += count)
        .or_insert(count);
}

fn runtime_active_bucket_label(bucket: RuntimeActiveBucket) -> &'static str {
    match bucket {
        RuntimeActiveBucket::Running => "running",
        RuntimeActiveBucket::Queued => "queued",
    }
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
                projection: WorkflowRuntimeTreeProjection::from_workflow(&instance),
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
                projection: WorkflowRuntimeTreeProjection::from_workflow(&instance),
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

#[cfg(test)]
fn apply_runtime_activity_summary(
    summary: &mut WorkflowRuntimeTreeSummary,
    jobs_by_command: &BTreeMap<String, Vec<RuntimeJob>>,
) {
    for job in jobs_by_command.values().flatten() {
        if let Some(outcome) = nodes::activity_result_envelope_from_job(job).and_then(|envelope| {
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

fn attach_workflow_children(
    workflow_id: &str,
    by_id: &mut BTreeMap<String, WorkflowRuntimeTreeNode>,
    children_by_parent: &BTreeMap<String, Vec<String>>,
) -> Option<WorkflowRuntimeTreeNode> {
    let mut node = by_id.remove(workflow_id)?;
    node.children = children_by_parent
        .get(workflow_id)
        .into_iter()
        .flatten()
        .filter_map(|child_id| attach_workflow_children(child_id, by_id, children_by_parent))
        .collect();
    Some(node)
}

#[cfg(test)]
#[path = "misc_routes_runtime_tree_tests.rs"]
mod runtime_tree_tests;
