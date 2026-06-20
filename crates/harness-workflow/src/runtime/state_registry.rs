use super::{
    pr_feedback::PR_FEEDBACK_DEFINITION_ID, prompt_task::PROMPT_TASK_DEFINITION_ID,
    quality_gate::QUALITY_GATE_DEFINITION_ID, reducer::GITHUB_ISSUE_PR_DEFINITION_ID,
    repo_backlog::REPO_BACKLOG_DEFINITION_ID,
};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowTerminalState {
    Succeeded,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct WorkflowStateKey {
    pub definition_id: &'static str,
    pub state: &'static str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkflowStateDefinition {
    pub key: WorkflowStateKey,
    pub terminal_state: Option<WorkflowTerminalState>,
}

impl WorkflowStateDefinition {
    pub const fn active(definition_id: &'static str, state: &'static str) -> Self {
        Self {
            key: WorkflowStateKey {
                definition_id,
                state,
            },
            terminal_state: None,
        }
    }

    pub const fn terminal(
        definition_id: &'static str,
        state: &'static str,
        terminal_state: WorkflowTerminalState,
    ) -> Self {
        Self {
            key: WorkflowStateKey {
                definition_id,
                state,
            },
            terminal_state: Some(terminal_state),
        }
    }
}

const WORKFLOW_DEFINITION_IDS: &[&str] = &[
    GITHUB_ISSUE_PR_DEFINITION_ID,
    PROMPT_TASK_DEFINITION_ID,
    QUALITY_GATE_DEFINITION_ID,
    PR_FEEDBACK_DEFINITION_ID,
    REPO_BACKLOG_DEFINITION_ID,
];

const GITHUB_ISSUE_PR_STATES: &[WorkflowStateDefinition] = &[
    WorkflowStateDefinition::active(GITHUB_ISSUE_PR_DEFINITION_ID, "discovered"),
    WorkflowStateDefinition::active(GITHUB_ISSUE_PR_DEFINITION_ID, "awaiting_dependencies"),
    WorkflowStateDefinition::active(GITHUB_ISSUE_PR_DEFINITION_ID, "scheduled"),
    WorkflowStateDefinition::active(GITHUB_ISSUE_PR_DEFINITION_ID, "planning"),
    WorkflowStateDefinition::active(GITHUB_ISSUE_PR_DEFINITION_ID, "implementing"),
    WorkflowStateDefinition::active(GITHUB_ISSUE_PR_DEFINITION_ID, "replanning"),
    WorkflowStateDefinition::active(GITHUB_ISSUE_PR_DEFINITION_ID, "pr_open"),
    WorkflowStateDefinition::active(GITHUB_ISSUE_PR_DEFINITION_ID, "local_review_gate"),
    WorkflowStateDefinition::active(GITHUB_ISSUE_PR_DEFINITION_ID, "awaiting_feedback"),
    WorkflowStateDefinition::active(GITHUB_ISSUE_PR_DEFINITION_ID, "addressing_feedback"),
    WorkflowStateDefinition::active(GITHUB_ISSUE_PR_DEFINITION_ID, "quality_gate_pending"),
    WorkflowStateDefinition::active(GITHUB_ISSUE_PR_DEFINITION_ID, "ready_to_merge"),
    WorkflowStateDefinition::active(GITHUB_ISSUE_PR_DEFINITION_ID, "blocked"),
    WorkflowStateDefinition::terminal(
        GITHUB_ISSUE_PR_DEFINITION_ID,
        "done",
        WorkflowTerminalState::Succeeded,
    ),
    WorkflowStateDefinition::terminal(
        GITHUB_ISSUE_PR_DEFINITION_ID,
        "failed",
        WorkflowTerminalState::Failed,
    ),
    WorkflowStateDefinition::terminal(
        GITHUB_ISSUE_PR_DEFINITION_ID,
        "cancelled",
        WorkflowTerminalState::Cancelled,
    ),
];

const PROMPT_TASK_STATES: &[WorkflowStateDefinition] = &[
    WorkflowStateDefinition::active(PROMPT_TASK_DEFINITION_ID, "submitted"),
    WorkflowStateDefinition::active(PROMPT_TASK_DEFINITION_ID, "awaiting_dependencies"),
    WorkflowStateDefinition::active(PROMPT_TASK_DEFINITION_ID, "implementing"),
    WorkflowStateDefinition::active(PROMPT_TASK_DEFINITION_ID, "blocked"),
    WorkflowStateDefinition::terminal(
        PROMPT_TASK_DEFINITION_ID,
        "done",
        WorkflowTerminalState::Succeeded,
    ),
    WorkflowStateDefinition::terminal(
        PROMPT_TASK_DEFINITION_ID,
        "failed",
        WorkflowTerminalState::Failed,
    ),
    WorkflowStateDefinition::terminal(
        PROMPT_TASK_DEFINITION_ID,
        "cancelled",
        WorkflowTerminalState::Cancelled,
    ),
];

const QUALITY_GATE_STATES: &[WorkflowStateDefinition] = &[
    WorkflowStateDefinition::active(QUALITY_GATE_DEFINITION_ID, "pending"),
    WorkflowStateDefinition::active(QUALITY_GATE_DEFINITION_ID, "checking"),
    WorkflowStateDefinition::active(QUALITY_GATE_DEFINITION_ID, "blocked"),
    WorkflowStateDefinition::terminal(
        QUALITY_GATE_DEFINITION_ID,
        "passed",
        WorkflowTerminalState::Succeeded,
    ),
    WorkflowStateDefinition::terminal(
        QUALITY_GATE_DEFINITION_ID,
        "failed",
        WorkflowTerminalState::Failed,
    ),
    WorkflowStateDefinition::terminal(
        QUALITY_GATE_DEFINITION_ID,
        "cancelled",
        WorkflowTerminalState::Cancelled,
    ),
];

const PR_FEEDBACK_STATES: &[WorkflowStateDefinition] = &[
    WorkflowStateDefinition::active(PR_FEEDBACK_DEFINITION_ID, "pending"),
    WorkflowStateDefinition::active(PR_FEEDBACK_DEFINITION_ID, "inspecting"),
    WorkflowStateDefinition::active(PR_FEEDBACK_DEFINITION_ID, "feedback_found"),
    WorkflowStateDefinition::active(PR_FEEDBACK_DEFINITION_ID, "no_actionable_feedback"),
    WorkflowStateDefinition::active(PR_FEEDBACK_DEFINITION_ID, "ready_to_merge"),
    WorkflowStateDefinition::active(PR_FEEDBACK_DEFINITION_ID, "blocked"),
    WorkflowStateDefinition::terminal(
        PR_FEEDBACK_DEFINITION_ID,
        "done",
        WorkflowTerminalState::Succeeded,
    ),
    WorkflowStateDefinition::terminal(
        PR_FEEDBACK_DEFINITION_ID,
        "failed",
        WorkflowTerminalState::Failed,
    ),
    WorkflowStateDefinition::terminal(
        PR_FEEDBACK_DEFINITION_ID,
        "cancelled",
        WorkflowTerminalState::Cancelled,
    ),
];

const REPO_BACKLOG_STATES: &[WorkflowStateDefinition] = &[
    WorkflowStateDefinition::active(REPO_BACKLOG_DEFINITION_ID, "idle"),
    WorkflowStateDefinition::active(REPO_BACKLOG_DEFINITION_ID, "scanning"),
    WorkflowStateDefinition::active(REPO_BACKLOG_DEFINITION_ID, "planning_batch"),
    WorkflowStateDefinition::active(REPO_BACKLOG_DEFINITION_ID, "dispatching"),
    WorkflowStateDefinition::active(REPO_BACKLOG_DEFINITION_ID, "reconciling"),
    WorkflowStateDefinition::active(REPO_BACKLOG_DEFINITION_ID, "blocked"),
    WorkflowStateDefinition::terminal(
        REPO_BACKLOG_DEFINITION_ID,
        "done",
        WorkflowTerminalState::Succeeded,
    ),
    WorkflowStateDefinition::terminal(
        REPO_BACKLOG_DEFINITION_ID,
        "failed",
        WorkflowTerminalState::Failed,
    ),
    WorkflowStateDefinition::terminal(
        REPO_BACKLOG_DEFINITION_ID,
        "cancelled",
        WorkflowTerminalState::Cancelled,
    ),
];

pub fn known_workflow_definition_ids() -> &'static [&'static str] {
    WORKFLOW_DEFINITION_IDS
}

pub fn workflow_states_for_definition(definition_id: &str) -> &'static [WorkflowStateDefinition] {
    match definition_id {
        GITHUB_ISSUE_PR_DEFINITION_ID => GITHUB_ISSUE_PR_STATES,
        PROMPT_TASK_DEFINITION_ID => PROMPT_TASK_STATES,
        QUALITY_GATE_DEFINITION_ID => QUALITY_GATE_STATES,
        PR_FEEDBACK_DEFINITION_ID => PR_FEEDBACK_STATES,
        REPO_BACKLOG_DEFINITION_ID => REPO_BACKLOG_STATES,
        _ => &[],
    }
}

pub fn workflow_terminal_state_names_for_definition(definition_id: &str) -> Vec<&'static str> {
    workflow_states_for_definition(definition_id)
        .iter()
        .filter_map(|definition| definition.terminal_state.map(|_| definition.key.state))
        .collect()
}

pub fn workflow_state_definition(
    definition_id: &str,
    state: &str,
) -> Option<&'static WorkflowStateDefinition> {
    workflow_states_for_definition(definition_id)
        .iter()
        .find(|definition| definition.key.state == state)
}

pub fn workflow_state_exists(definition_id: &str, state: &str) -> bool {
    workflow_state_definition(definition_id, state).is_some()
}

pub fn workflow_state_terminal_state(
    definition_id: &str,
    state: &str,
) -> Option<WorkflowTerminalState> {
    workflow_state_definition(definition_id, state)?.terminal_state
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::validator::TransitionAllowlist;

    #[test]
    fn registry_scopes_success_states_to_workflow_definitions() {
        assert_eq!(
            workflow_state_terminal_state(QUALITY_GATE_DEFINITION_ID, "passed"),
            Some(WorkflowTerminalState::Succeeded)
        );
        assert_eq!(
            workflow_state_terminal_state(GITHUB_ISSUE_PR_DEFINITION_ID, "passed"),
            None
        );
        assert_eq!(
            workflow_state_terminal_state(QUALITY_GATE_DEFINITION_ID, "done"),
            None
        );
    }

    #[test]
    fn registry_lists_only_known_definition_states() {
        assert!(known_workflow_definition_ids().contains(&GITHUB_ISSUE_PR_DEFINITION_ID));
        assert!(workflow_states_for_definition("unknown_workflow").is_empty());
        assert!(workflow_state_exists(
            GITHUB_ISSUE_PR_DEFINITION_ID,
            "awaiting_feedback"
        ));
        assert!(!workflow_state_exists(
            GITHUB_ISSUE_PR_DEFINITION_ID,
            "inspecting"
        ));
    }

    #[test]
    fn registry_lists_terminal_state_names_by_definition() {
        assert_eq!(
            workflow_terminal_state_names_for_definition(GITHUB_ISSUE_PR_DEFINITION_ID),
            vec!["done", "failed", "cancelled"]
        );
        assert_eq!(
            workflow_terminal_state_names_for_definition(QUALITY_GATE_DEFINITION_ID),
            vec!["passed", "failed", "cancelled"]
        );
        assert!(workflow_terminal_state_names_for_definition("unknown_workflow").is_empty());
    }

    #[test]
    fn registry_covers_validator_transition_states() {
        let allowlists = [
            (
                GITHUB_ISSUE_PR_DEFINITION_ID,
                TransitionAllowlist::github_issue_pr_defaults(),
            ),
            (
                PROMPT_TASK_DEFINITION_ID,
                TransitionAllowlist::prompt_task_defaults(),
            ),
            (
                QUALITY_GATE_DEFINITION_ID,
                TransitionAllowlist::quality_gate_defaults(),
            ),
            (
                PR_FEEDBACK_DEFINITION_ID,
                TransitionAllowlist::pr_feedback_defaults(),
            ),
            (
                REPO_BACKLOG_DEFINITION_ID,
                TransitionAllowlist::repo_backlog_defaults(),
            ),
        ];

        for (definition_id, allowlist) in allowlists {
            for rule in allowlist.rules() {
                if let Some(from_state) = rule.from_state.as_deref() {
                    assert!(
                        workflow_state_exists(definition_id, from_state),
                        "{definition_id} missing from_state {from_state}"
                    );
                }
                assert!(
                    workflow_state_exists(definition_id, &rule.to_state),
                    "{definition_id} missing to_state {}",
                    rule.to_state
                );
            }
        }
    }
}
