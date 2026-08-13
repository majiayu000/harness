use super::{string_field, u64_field, workflow_source, OperatorAction};
use crate::runtime_projection::{
    RuntimeStoppedActionEligibility, RuntimeStoppedStateProjection, RuntimeWorkflowProjection,
};
use chrono::{DateTime, Utc};
use harness_workflow::runtime::{
    WorkflowInstance, WorkflowTerminalState, GITHUB_ISSUE_PR_DEFINITION_ID,
};
use std::collections::HashMap;

pub(super) fn operator_actions(
    workflows: &[WorkflowInstance],
    generated_at: DateTime<Utc>,
    stopped_eligibility: &HashMap<String, RuntimeStoppedActionEligibility>,
) -> Vec<OperatorAction> {
    let mut actions = Vec::new();
    for workflow in workflows {
        let eligibility = stopped_eligibility
            .get(&workflow.id)
            .copied()
            .unwrap_or_default();
        let kind = if workflow.definition_id == GITHUB_ISSUE_PR_DEFINITION_ID
            && workflow.state == "awaiting_dependencies"
        {
            eligibility.can_unblock.then_some("blocked")
        } else {
            workflow_action_kind(workflow)
        };
        let Some(kind) = kind else { continue };
        let projection = RuntimeWorkflowProjection::from_workflow_with_stopped_eligibility(
            workflow,
            eligibility,
        );
        let next_action = workflow_next_action(kind, &projection.stopped_state);
        let task_id = projection
            .legacy_dedupe_task_handle
            .as_ref()
            .map(|task_id| task_id.0.clone())
            .or_else(|| {
                projection
                    .submission_handle
                    .as_ref()
                    .map(|task_id| task_id.as_str().to_string())
            });
        let repo = string_field(&workflow.data, "repo");
        let issue = u64_field(&workflow.data, "issue_number");
        let pr = u64_field(&workflow.data, "pr_number");
        let pr_url = string_field(&workflow.data, "pr_url");
        let issue_url = repo
            .as_ref()
            .zip(issue)
            .map(|(repo, issue)| format!("https://github.com/{repo}/issues/{issue}"));
        actions.push(OperatorAction {
            kind,
            repo,
            issue,
            pr,
            evidence_url: task_id
                .as_ref()
                .map(|id| format!("/api/workflows/runtime/submissions/{id}")),
            task_id,
            workflow_id: workflow.id.clone(),
            state: workflow.state.clone(),
            age_secs: generated_at
                .signed_duration_since(workflow.updated_at)
                .num_seconds()
                .max(0) as u64,
            url: pr_url.or(issue_url),
            next_action,
            source: workflow_source(workflow),
            stopped_state: projection.stopped_state,
        });
    }
    actions.sort_by(|a, b| {
        action_priority(a.kind)
            .cmp(&action_priority(b.kind))
            .then_with(|| b.age_secs.cmp(&a.age_secs))
    });
    actions.truncate(super::MAX_OPERATOR_ACTIONS);
    actions
}

pub(super) fn workflow_action_kind(workflow: &WorkflowInstance) -> Option<&'static str> {
    if workflow.terminal_state() == Some(WorkflowTerminalState::Failed) {
        return Some("failed");
    }
    if workflow.definition_id == GITHUB_ISSUE_PR_DEFINITION_ID
        && workflow.state == "awaiting_dependencies"
    {
        return Some("blocked");
    }
    if harness_workflow::runtime::declarative_workflow_definition_for_instance(workflow).is_some() {
        return harness_workflow::runtime::workflow_state_definition_for_instance(
            workflow,
            &workflow.state,
        )
        .is_some_and(|state| {
            state.progress_mode
                == Some(harness_workflow::runtime::WorkflowProgressMode::OperatorGate)
        })
        .then_some("blocked");
    }
    match workflow.state.as_str() {
        "ready_to_merge" => Some("ready_to_merge"),
        "awaiting_feedback" => Some("awaiting_feedback"),
        "blocked" => Some("blocked"),
        _ => None,
    }
}

fn workflow_next_action(kind: &str, stopped_state: &RuntimeStoppedStateProjection) -> &'static str {
    match kind {
        "ready_to_merge" => "Review and merge",
        "awaiting_feedback" => "Inspect review feedback",
        "blocked" => "Resolve blocker",
        "failed" if stopped_state.can_retry => "Retry failed workflow",
        "failed" => "Inspect failed workflow",
        _ => "Inspect workflow",
    }
}

fn action_priority(kind: &str) -> u8 {
    match kind {
        "ready_to_merge" => 0,
        "blocked" => 1,
        "failed" => 2,
        "awaiting_feedback" => 3,
        _ => 3,
    }
}
