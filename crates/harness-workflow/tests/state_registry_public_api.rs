use harness_workflow::runtime::{
    register_workflow_definition, workflow_definition, RegisteredWorkflowDefinition,
    TransitionAllowlist, TransitionRule, WorkflowDefinition as PersistedWorkflowDefinition,
    WorkflowStateDefinition,
};

#[test]
fn downstream_crate_can_construct_and_register_a_runtime_definition() {
    let definition_id = "downstream_registry_api_fixture";
    let definition = RegisteredWorkflowDefinition::new(
        definition_id,
        vec![
            WorkflowStateDefinition::active(definition_id, "pending"),
            WorkflowStateDefinition::active(definition_id, "running"),
        ],
        TransitionAllowlist::new(vec![TransitionRule::new("pending", "running", [])]),
    );

    register_workflow_definition(definition)
        .expect("downstream runtime definition should register through the public API");

    let registered = workflow_definition(definition_id)
        .expect("downstream runtime definition should be available after registration");
    assert_eq!(registered.id, definition_id);
    assert_eq!(registered.states.len(), 2);

    let persisted = PersistedWorkflowDefinition::new(definition_id, 1, "Downstream fixture");
    assert_eq!(persisted.id, registered.id);
}
