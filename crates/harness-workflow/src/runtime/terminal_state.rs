use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowTerminalState {
    Succeeded,
    Failed,
    Cancelled,
}

pub fn workflow_terminal_state(definition_id: &str, state: &str) -> Option<WorkflowTerminalState> {
    match (definition_id, state) {
        (_, "done" | "passed") => Some(WorkflowTerminalState::Succeeded),
        (_, "failed") => Some(WorkflowTerminalState::Failed),
        (_, "cancelled") => Some(WorkflowTerminalState::Cancelled),
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
    fn terminal_state_preserves_passed_as_shared_success_terminal_state() {
        assert_eq!(
            workflow_terminal_state("quality_gate", "passed"),
            Some(WorkflowTerminalState::Succeeded)
        );
        assert_eq!(
            workflow_terminal_state("github_issue_pr", "passed"),
            Some(WorkflowTerminalState::Succeeded)
        );
    }
}
