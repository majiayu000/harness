use crate::task_runner::TaskId;
use harness_workflow::runtime::{
    WorkflowCommandStatus, WorkflowDecision, WorkflowDecisionRecord, WorkflowRuntimeStore,
};

pub(super) struct SubmissionEventSelection {
    pub(super) event_id: String,
    pub(super) has_prior_attempt: bool,
    pub(super) replay_existing: bool,
}

impl SubmissionEventSelection {
    pub(super) fn disambiguates_command_dedupe(&self) -> bool {
        self.has_prior_attempt && !self.replay_existing
    }
}

pub(super) struct SubmissionEventLookup {
    pub(super) event_id: Option<String>,
    pub(super) has_prior_attempt: bool,
}

pub(super) async fn submission_event_for_replay(
    store: &WorkflowRuntimeStore,
    workflow_id: &str,
    event_type: &str,
    task_id: &TaskId,
) -> anyhow::Result<SubmissionEventLookup> {
    let task_id = task_id.as_str();
    let mut has_prior_attempt = false;
    for event in store.events_for(workflow_id).await?.into_iter().rev() {
        if event.event_type != event_type
            || event
                .event
                .get("task_id")
                .and_then(serde_json::Value::as_str)
                .is_none_or(|recorded| recorded != task_id)
        {
            continue;
        }
        has_prior_attempt = true;
        if submission_event_needs_replay(store, workflow_id, &event.id).await? {
            return Ok(SubmissionEventLookup {
                event_id: Some(event.id),
                has_prior_attempt,
            });
        }
    }
    Ok(SubmissionEventLookup {
        event_id: None,
        has_prior_attempt,
    })
}

async fn submission_event_needs_replay(
    store: &WorkflowRuntimeStore,
    workflow_id: &str,
    event_id: &str,
) -> anyhow::Result<bool> {
    let Some(record) = decision_for_event(store, workflow_id, event_id).await? else {
        return Ok(true);
    };
    if !record.accepted {
        return Ok(false);
    }
    let instance = store.get_instance(workflow_id).await?;
    if let Some(instance) = &instance {
        if instance.state != record.decision.observed_state
            && instance.state != record.decision.next_state
        {
            return Ok(false);
        }
    }
    let state_needs_replay = match &instance {
        Some(instance) => instance.state != record.decision.next_state,
        None => true,
    };
    let commands = store.commands_for(workflow_id).await?;
    for expected in &record.decision.commands {
        let Some(command) = commands.iter().find(|command| {
            command.decision_id.as_deref() == Some(record.id.as_str())
                && command.command.command_type == expected.command_type
                && command.command.dedupe_key == expected.dedupe_key
        }) else {
            return Ok(true);
        };
        if is_replayable_command_status(command.status) {
            return Ok(true);
        }
    }
    Ok(state_needs_replay)
}

fn is_replayable_command_status(status: WorkflowCommandStatus) -> bool {
    matches!(
        status,
        WorkflowCommandStatus::Pending
            | WorkflowCommandStatus::Dispatching
            | WorkflowCommandStatus::Dispatched
    )
}

pub(super) fn disambiguate_submission_command_dedupe(
    decision: &mut WorkflowDecision,
    event_id: &str,
) {
    for command in &mut decision.commands {
        command.dedupe_key = format!("{}:event:{}", command.dedupe_key, event_id);
    }
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
