use harness_workflow::runtime::{
    RegisteredWorkflowDefinition, TransitionAllowlist, TransitionRule,
    WorkflowDefinition as PersistedWorkflowDefinition, WorkflowDefinitionRegistry,
    WorkflowProgressMode, WorkflowStateDefinition, WorkflowTerminalState,
};

#[test]
fn downstream_crate_can_construct_and_register_a_runtime_definition() {
    let definition_id = "downstream_registry_api_fixture";
    let definition = RegisteredWorkflowDefinition::new(
        definition_id,
        vec![
            WorkflowStateDefinition::active(
                definition_id,
                "pending",
                WorkflowProgressMode::ExternalWait,
            ),
            WorkflowStateDefinition::active(
                definition_id,
                "running",
                WorkflowProgressMode::OperatorGate,
            ),
        ],
        TransitionAllowlist::new(vec![TransitionRule::new("pending", "running", [])]),
    );

    let mut registry = WorkflowDefinitionRegistry::new();
    registry
        .register(definition)
        .expect("downstream runtime definition should register through the public API");

    let registered = registry
        .definition(definition_id)
        .expect("downstream runtime definition should be available after registration");
    assert_eq!(registered.id, definition_id);
    assert_eq!(registered.states.len(), 2);
    assert_eq!(
        registered.states[1].progress_mode,
        Some(WorkflowProgressMode::OperatorGate)
    );

    let persisted = PersistedWorkflowDefinition::new(definition_id, 1, "Downstream fixture");
    assert_eq!(persisted.id, registered.id);
}

#[test]
fn independent_registries_can_resolve_concurrently_without_cross_contamination() {
    fn registry_with_terminal(
        state: &str,
        terminal: WorkflowTerminalState,
    ) -> std::sync::Arc<WorkflowDefinitionRegistry> {
        let mut registry = WorkflowDefinitionRegistry::new();
        registry
            .register(RegisteredWorkflowDefinition::new(
                "isolated_definition",
                vec![WorkflowStateDefinition::terminal(
                    "isolated_definition",
                    state,
                    terminal,
                )],
                TransitionAllowlist::default(),
            ))
            .expect("isolated definition should register");
        registry.into_shared()
    }

    let succeeded = registry_with_terminal("complete", WorkflowTerminalState::Succeeded);
    let failed = registry_with_terminal("rejected", WorkflowTerminalState::Failed);
    let succeeded_thread = std::thread::spawn(move || {
        succeeded.state_terminal_state("isolated_definition", "complete")
    });
    let failed_thread =
        std::thread::spawn(move || failed.state_terminal_state("isolated_definition", "rejected"));

    assert_eq!(
        succeeded_thread
            .join()
            .expect("success lookup should finish"),
        Some(WorkflowTerminalState::Succeeded)
    );
    assert_eq!(
        failed_thread.join().expect("failure lookup should finish"),
        Some(WorkflowTerminalState::Failed)
    );
}
