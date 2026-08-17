pub use super::state_registry::WorkflowTerminalState;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_state_treats_done_failed_and_cancelled_as_shared_terminal_states() {
        let registry = super::super::state_registry::WorkflowDefinitionRegistry::with_builtins();
        assert_eq!(
            registry.state_terminal_state("github_issue_pr", "done"),
            Some(WorkflowTerminalState::Succeeded)
        );
        assert_eq!(
            registry.state_terminal_state("prompt_task", "failed"),
            Some(WorkflowTerminalState::Failed)
        );
        assert_eq!(
            registry.state_terminal_state("pr_feedback", "cancelled"),
            Some(WorkflowTerminalState::Cancelled)
        );
    }

    #[test]
    fn terminal_state_scopes_success_states_to_workflow_definitions() {
        let registry = super::super::state_registry::WorkflowDefinitionRegistry::with_builtins();
        assert_eq!(
            registry.state_terminal_state("quality_gate", "passed"),
            Some(WorkflowTerminalState::Succeeded)
        );
        assert_eq!(
            registry.state_terminal_state("github_issue_pr", "passed"),
            None
        );
        assert_eq!(registry.state_terminal_state("quality_gate", "done"), None);
    }

    #[test]
    fn terminal_state_rejects_terminal_looking_states_for_unknown_definitions() {
        let registry = super::super::state_registry::WorkflowDefinitionRegistry::with_builtins();
        for state in ["done", "passed", "failed", "cancelled"] {
            assert_eq!(
                registry.state_terminal_state("unknown_workflow", state),
                None
            );
        }
    }
}
