use super::{workflow_legacy_task_ids, workflow_source, TaskSummary, WorkflowInstance};
use crate::runtime_projection::{RuntimeActiveBucket, RuntimeWorkflowProjection};
use crate::task_runner::{SchedulerAuthorityState, TaskPhase, TaskStatus};
use harness_workflow::runtime::{
    declarative_workflow_definition_for_instance, workflow_state_definition_for_instance,
    WorkflowProgressMode,
};
use serde::Serialize;
use std::collections::HashMap;

#[derive(Debug, Default, Clone, Serialize, PartialEq, Eq)]
pub(super) struct RuntimeWorkflowCounts {
    pub(super) pending: u64,
    pub(super) running: u64,
    pub(super) review: u64,
    pub(super) awaiting_dependencies: u64,
    pub(super) ready_to_merge: u64,
    pub(super) blocked: u64,
    pub(super) failed: u64,
    pub(super) done: u64,
    pub(super) other: u64,
}

#[derive(Debug, Default, Clone, Serialize, PartialEq, Eq)]
pub(super) struct SourceActivity {
    pub(super) source: String,
    pub(super) pending: u64,
    pub(super) running: u64,
    pub(super) review: u64,
    pub(super) blocked: u64,
    pub(super) failed: u64,
    pub(super) ready_to_merge: u64,
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

pub(super) fn runtime_workflow_counts(workflows: &[WorkflowInstance]) -> RuntimeWorkflowCounts {
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

fn workflow_bucket(workflow: &WorkflowInstance) -> WorkflowBucket {
    let projection = RuntimeWorkflowProjection::from_workflow(workflow);
    match projection.task_status {
        TaskStatus::Failed => return WorkflowBucket::Failed,
        TaskStatus::Done => return WorkflowBucket::Done,
        TaskStatus::AwaitingDeps => return WorkflowBucket::AwaitingDependencies,
        _ => {}
    }
    if declarative_workflow_definition_for_instance(workflow).is_some()
        && workflow_state_definition_for_instance(workflow, &workflow.state)
            .is_some_and(|state| state.progress_mode == Some(WorkflowProgressMode::OperatorGate))
    {
        return WorkflowBucket::Blocked;
    }
    if workflow.state == "ready_to_merge" {
        return WorkflowBucket::ReadyToMerge;
    }
    if workflow.state == "blocked" {
        return WorkflowBucket::Blocked;
    }
    if projection.phase == TaskPhase::Review {
        return WorkflowBucket::Review;
    }
    match projection.active_bucket() {
        Some(RuntimeActiveBucket::Running) => WorkflowBucket::Running,
        Some(RuntimeActiveBucket::Queued) => WorkflowBucket::Pending,
        None => WorkflowBucket::Other,
    }
}

pub(super) fn source_activity(
    workflows: &[WorkflowInstance],
    active_tasks: &[TaskSummary],
) -> Vec<SourceActivity> {
    let mut by_source: HashMap<String, SourceActivity> = HashMap::new();
    let workflow_task_ids = workflow_legacy_task_ids(workflows);
    for workflow in workflows {
        add_workflow_source_activity(&mut by_source, workflow);
    }
    for task in active_tasks {
        if task.workflow.is_some() || workflow_task_ids.contains(task.id.as_str()) {
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
    let bucket = workflow_bucket(workflow);
    if matches!(bucket, WorkflowBucket::Done | WorkflowBucket::Other) {
        return;
    }
    let source = workflow_source(workflow);
    let entry = by_source
        .entry(source.clone())
        .or_insert_with(|| SourceActivity {
            source,
            ..Default::default()
        });
    match bucket {
        WorkflowBucket::Pending => entry.pending += 1,
        WorkflowBucket::Running => entry.running += 1,
        WorkflowBucket::Review => entry.review += 1,
        WorkflowBucket::AwaitingDependencies | WorkflowBucket::Blocked => entry.blocked += 1,
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
    source.pending
        + source.running
        + source.review
        + source.blocked
        + source.failed
        + source.ready_to_merge
}
