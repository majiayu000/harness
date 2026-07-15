use crate::runtime::state_registry::{
    known_workflow_definition_ids, workflow_terminal_state_names_for_definition,
};
use crate::runtime::WorkflowOtelTraceContext;

pub(super) fn otel_trace_context_from_data(
    data: &serde_json::Value,
) -> Option<WorkflowOtelTraceContext> {
    let context =
        serde_json::from_value::<WorkflowOtelTraceContext>(data.get("otel_trace_context")?.clone())
            .ok()?;
    context.has_valid_trace_ids().then_some(context)
}

pub(super) fn terminal_state_pairs() -> (Vec<String>, Vec<String>) {
    let mut definition_ids = Vec::new();
    let mut states = Vec::new();
    for definition_id in known_workflow_definition_ids() {
        for state in workflow_terminal_state_names_for_definition(&definition_id) {
            definition_ids.push(definition_id.clone());
            states.push(state);
        }
    }
    (definition_ids, states)
}
