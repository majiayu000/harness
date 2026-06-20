pub use super::state_registry::WorkflowTerminalState;

pub fn workflow_terminal_state(definition_id: &str, state: &str) -> Option<WorkflowTerminalState> {
    super::state_registry::workflow_state_terminal_state(definition_id, state)
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
