use crate::task_runner::TaskId;
use harness_workflow::runtime::{
    RuntimeJob, WorkflowDecisionRecord, WorkflowInstance, WorkflowRuntimeStore,
};
use serde_json::Value;

use super::data_helpers::activity_name;

pub(super) async fn ensure_runtime_job_still_owns_lease(
    store: &WorkflowRuntimeStore,
    job: &RuntimeJob,
) -> anyhow::Result<()> {
    if store.runtime_job_matches_running_lease(job).await? {
        return Ok(());
    }
    anyhow::bail!(
        "runtime job `{}` no longer owns the current lease for `{}`",
        job.id,
        activity_name(job)
    )
}

pub(super) async fn child_start_event_recorded(
    store: &WorkflowRuntimeStore,
    child_id: &str,
    command_id: &str,
) -> anyhow::Result<bool> {
    Ok(store.events_for(child_id).await?.iter().any(|event| {
        event.event_type == "ChildWorkflowStarted"
            && event
                .event
                .get("command_id")
                .and_then(Value::as_str)
                .is_some_and(|recorded| recorded == command_id)
    }))
}

pub(super) async fn child_event_id_or_append(
    store: &WorkflowRuntimeStore,
    child_id: &str,
    event_type: &str,
    payload: Value,
) -> anyhow::Result<String> {
    if let Some(event_id) = child_event_id(store, child_id, event_type).await? {
        return Ok(event_id);
    }
    Ok(store
        .append_event(child_id, event_type, "workflow_runtime_worker", payload)
        .await?
        .id)
}

pub(super) async fn decision_for_event(
    store: &WorkflowRuntimeStore,
    workflow_id: &str,
    event_id: &str,
) -> anyhow::Result<Option<WorkflowDecisionRecord>> {
    Ok(store
        .decisions_for(workflow_id)
        .await?
        .into_iter()
        .find(|record| record.event_id.as_deref() == Some(event_id)))
}

async fn child_event_id(
    store: &WorkflowRuntimeStore,
    child_id: &str,
    event_type: &str,
) -> anyhow::Result<Option<String>> {
    Ok(store
        .events_for(child_id)
        .await?
        .into_iter()
        .find(|event| event.event_type == event_type)
        .map(|event| event.id))
}

pub(super) fn child_started_by_command(child: &WorkflowInstance, command_id: &str) -> bool {
    child
        .data
        .get("started_by_command_id")
        .and_then(Value::as_str)
        .is_some_and(|recorded| recorded == command_id)
}

fn issue_submission_data_recorded(child: &WorkflowInstance, task_id: &TaskId) -> bool {
    let task_id = task_id.as_str();
    child
        .data
        .get("submission_id")
        .or_else(|| child.data.get("task_id"))
        .and_then(Value::as_str)
        .is_some_and(|recorded| recorded == task_id)
        || child
            .data
            .get("task_ids")
            .and_then(Value::as_array)
            .is_some_and(|task_ids| {
                task_ids
                    .iter()
                    .filter_map(Value::as_str)
                    .any(|recorded| recorded == task_id)
            })
}

async fn issue_submission_event_recorded(
    store: &WorkflowRuntimeStore,
    child_id: &str,
    task_id: &TaskId,
) -> anyhow::Result<Option<String>> {
    let task_id = task_id.as_str();
    Ok(store
        .events_for(child_id)
        .await?
        .into_iter()
        .find(|event| {
            event.event_type == "IssueSubmitted"
                && event
                    .event
                    .get("task_id")
                    .and_then(Value::as_str)
                    .is_some_and(|recorded| recorded == task_id)
        })
        .map(|event| event.id))
}

async fn issue_submission_side_effects_recorded(
    store: &WorkflowRuntimeStore,
    child: &WorkflowInstance,
    task_id: &TaskId,
) -> anyhow::Result<bool> {
    let Some(event_id) = issue_submission_event_recorded(store, &child.id, task_id).await? else {
        return Ok(false);
    };
    let decisions = store.decisions_for(&child.id).await?;
    let Some(decision) = decisions
        .iter()
        .find(|record| record.accepted && record.event_id.as_deref() == Some(event_id.as_str()))
    else {
        return Ok(false);
    };
    if child.state != decision.decision.next_state {
        return Ok(false);
    }
    let commands = store.commands_for(&child.id).await?;
    Ok(decision.decision.commands.iter().all(|expected| {
        commands.iter().any(|record| {
            record.decision_id.as_deref() == Some(decision.id.as_str())
                && record.command.command_type == expected.command_type
                && record.command.dedupe_key == expected.dedupe_key
        })
    }))
}

pub(super) async fn issue_submission_recorded(
    store: &WorkflowRuntimeStore,
    child: &WorkflowInstance,
    task_id: &TaskId,
) -> anyhow::Result<bool> {
    if issue_submission_data_recorded(child, task_id) {
        return Ok(true);
    }
    issue_submission_side_effects_recorded(store, child, task_id).await
}
