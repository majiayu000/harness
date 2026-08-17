use super::*;
use crate::runtime::{
    RegisteredWorkflowDefinition, TransitionAllowlist, WorkflowDefinitionRegistry,
    WorkflowStateDefinition, WorkflowSubject,
};

#[test]
fn repo_memory_outcome_uses_injected_custom_terminal_class() -> anyhow::Result<()> {
    let definition_id = "repo_memory_injected_terminal";
    let mut registry = WorkflowDefinitionRegistry::with_builtins();
    registry.register(RegisteredWorkflowDefinition::new(
        definition_id,
        vec![
            WorkflowStateDefinition::terminal(
                definition_id,
                "shipped",
                WorkflowTerminalState::Succeeded,
            ),
            WorkflowStateDefinition::terminal(
                definition_id,
                "rejected",
                WorkflowTerminalState::Failed,
            ),
        ],
        TransitionAllowlist::default(),
    ))?;
    let instance =
        |state| WorkflowInstance::new(definition_id, 1, state, WorkflowSubject::new("test", state));

    assert_eq!(
        repo_memory_outcome(&registry, &instance("shipped")),
        Some(RepoMemoryOutcome::Done)
    );
    assert_eq!(
        repo_memory_outcome(&registry, &instance("rejected")),
        Some(RepoMemoryOutcome::Failed)
    );
    Ok(())
}
