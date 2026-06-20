use serde::{Deserialize, Serialize};

use super::{
    pr_feedback::PR_FEEDBACK_DEFINITION_ID, prompt_task::PROMPT_TASK_DEFINITION_ID,
    quality_gate::QUALITY_GATE_DEFINITION_ID, reducer::GITHUB_ISSUE_PR_DEFINITION_ID,
    repo_backlog::REPO_BACKLOG_DEFINITION_ID,
};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowTerminalState {
    Succeeded,
    Failed,
    Cancelled,
}

pub fn workflow_terminal_state(definition_id: &str, state: &str) -> Option<WorkflowTerminalState> {
    use WorkflowTerminalState::{Cancelled, Failed, Succeeded};

    match (definition_id, state) {
        (
            GITHUB_ISSUE_PR_DEFINITION_ID
            | PROMPT_TASK_DEFINITION_ID
            | REPO_BACKLOG_DEFINITION_ID
            | PR_FEEDBACK_DEFINITION_ID,
            "done",
        ) => Some(Succeeded),
        (QUALITY_GATE_DEFINITION_ID, "passed") => Some(Succeeded),
        (
            GITHUB_ISSUE_PR_DEFINITION_ID
            | PROMPT_TASK_DEFINITION_ID
            | REPO_BACKLOG_DEFINITION_ID
            | PR_FEEDBACK_DEFINITION_ID
            | QUALITY_GATE_DEFINITION_ID,
            "failed",
        ) => Some(Failed),
        (
            GITHUB_ISSUE_PR_DEFINITION_ID
            | PROMPT_TASK_DEFINITION_ID
            | REPO_BACKLOG_DEFINITION_ID
            | PR_FEEDBACK_DEFINITION_ID
            | QUALITY_GATE_DEFINITION_ID,
            "cancelled",
        ) => Some(Cancelled),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_state_treats_done_failed_and_cancelled_as_shared_terminal_states() {
        assert_eq!(
            workflow_terminal_state("github_issue_pr", "done"),
            Some(WorkflowTerminalState::Succeeded)
        );
        assert_eq!(
            workflow_terminal_state("prompt_task", "failed"),
            Some(WorkflowTerminalState::Failed)
        );
        assert_eq!(
            workflow_terminal_state("repo_backlog", "cancelled"),
            Some(WorkflowTerminalState::Cancelled)
        );
    }

    #[test]
    fn terminal_state_scopes_success_states_to_workflow_definitions() {
        assert_eq!(
            workflow_terminal_state("quality_gate", "passed"),
            Some(WorkflowTerminalState::Succeeded)
        );
        assert_eq!(workflow_terminal_state("github_issue_pr", "passed"), None);
        assert_eq!(workflow_terminal_state("quality_gate", "done"), None);
    }

    #[test]
    fn terminal_state_rejects_terminal_looking_states_for_unknown_definitions() {
        for state in ["done", "passed", "failed", "cancelled"] {
            assert_eq!(workflow_terminal_state("unknown_workflow", state), None);
        }
    }
}
