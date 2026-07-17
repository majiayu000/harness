use super::{OPERATOR_ACTION_STATES, WORKFLOW_SAMPLE_LIMIT};
use harness_workflow::runtime::{WorkflowInstance, WorkflowRuntimeStore, WorkflowTerminalState};
use std::cmp::Reverse;
use std::collections::HashSet;

pub(super) async fn list_operator_action_workflows(
    store: &WorkflowRuntimeStore,
    definition_ids: &[String],
) -> anyhow::Result<Vec<WorkflowInstance>> {
    let mut workflows = Vec::new();
    for definition_id in definition_ids {
        for state in OPERATOR_ACTION_STATES {
            workflows.extend(
                store
                    .list_recent_instances_by_state(definition_id, state, WORKFLOW_SAMPLE_LIMIT)
                    .await?,
            );
        }
    }
    Ok(workflows)
}

pub(super) fn dedupe_workflows(workflows: &mut Vec<WorkflowInstance>) {
    let mut seen = HashSet::new();
    workflows.retain(|workflow| seen.insert(workflow.id.clone()));
}

pub(super) async fn list_recent_failed_workflows(
    store: &WorkflowRuntimeStore,
    definition_ids: &[String],
    capacity: usize,
) -> anyhow::Result<Vec<WorkflowInstance>> {
    if capacity == 0 {
        return Ok(Vec::new());
    }
    let per_definition_limit = capacity.min(WORKFLOW_SAMPLE_LIMIT as usize) as i64;
    let mut workflows = Vec::new();
    for definition_id in definition_ids {
        workflows.extend(
            store
                .list_recent_terminal_instances_by_definition(
                    definition_id,
                    WorkflowTerminalState::Failed,
                    per_definition_limit,
                )
                .await?,
        );
    }
    workflows.sort_by_key(|workflow| Reverse(workflow.updated_at));
    workflows.truncate(capacity);
    Ok(workflows)
}
