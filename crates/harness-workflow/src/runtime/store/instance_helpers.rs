use crate::runtime::{WorkflowDefinitionRegistry, WorkflowOtelTraceContext, WorkflowTerminalState};

pub(super) fn otel_trace_context_from_data(
    data: &serde_json::Value,
) -> Option<WorkflowOtelTraceContext> {
    let context =
        serde_json::from_value::<WorkflowOtelTraceContext>(data.get("otel_trace_context")?.clone())
            .ok()?;
    context.has_valid_trace_ids().then_some(context)
}

pub(super) struct TerminalStateSelectorRows {
    pub(super) definition_ids: Vec<String>,
    pub(super) definition_versions: Vec<Option<i64>>,
    pub(super) definition_hashes: Vec<Option<String>>,
    pub(super) states: Vec<String>,
}

pub(super) fn terminal_state_selector_rows(
    registry: &WorkflowDefinitionRegistry,
) -> TerminalStateSelectorRows {
    let mut definition_ids = Vec::new();
    let mut definition_versions = Vec::new();
    let mut definition_hashes = Vec::new();
    let mut states = Vec::new();
    for definition_id in registry.known_definition_ids() {
        for selector in registry.terminal_state_selectors(&definition_id) {
            definition_ids.push(definition_id.clone());
            definition_versions.push(selector.definition_version.map(i64::from));
            definition_hashes.push(selector.definition_hash);
            states.push(selector.state);
        }
    }
    TerminalStateSelectorRows {
        definition_ids,
        definition_versions,
        definition_hashes,
        states,
    }
}

pub(super) fn terminal_task_status_rows(
    registry: &WorkflowDefinitionRegistry,
) -> (Vec<String>, Vec<String>, Vec<String>) {
    let mut definition_ids = Vec::new();
    let mut states = Vec::new();
    let mut task_statuses = Vec::new();
    for definition_id in registry.known_definition_ids() {
        for selector in registry.terminal_state_selectors(&definition_id) {
            // Declarative versions are joined through persisted definition
            // metadata by the submission queries. Keeping them out of this
            // unversioned CTE prevents current policy from overriding a
            // historical pin that reused the same state name.
            if selector.definition_version.is_some() || selector.definition_hash.is_some() {
                continue;
            }
            definition_ids.push(definition_id.clone());
            states.push(selector.state);
            task_statuses.push(
                match selector.terminal_state {
                    WorkflowTerminalState::Succeeded => "done",
                    WorkflowTerminalState::Failed => "failed",
                    WorkflowTerminalState::Cancelled => "cancelled",
                }
                .to_string(),
            );
        }
    }
    (definition_ids, states, task_statuses)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::build_declarative_definition;
    use harness_core::config::workflow::{
        DeclaredProgressMode, DeclaredState, WorkflowActivityPolicy, WorkflowDefinitionPolicy,
    };
    use std::collections::BTreeMap;

    #[test]
    fn declarative_terminal_rows_preserve_version_and_hash() -> anyhow::Result<()> {
        let definition_id = "terminal_selector_pin_fixture";
        let definition = build_declarative_definition(
            &WorkflowDefinitionPolicy {
                id: definition_id.to_string(),
                initial: "work".to_string(),
                states: BTreeMap::from([
                    (
                        "work".to_string(),
                        DeclaredState {
                            activity: Some("run".to_string()),
                            on_success: Some("done".to_string()),
                            on_failure: Some("failed".to_string()),
                            ..DeclaredState::default()
                        },
                    ),
                    (
                        "blocked".to_string(),
                        DeclaredState {
                            progress: Some(DeclaredProgressMode::OperatorGate),
                            ..DeclaredState::default()
                        },
                    ),
                ]),
                terminal: BTreeMap::from([
                    ("cancelled".to_string(), "cancelled".to_string()),
                    ("done".to_string(), "succeeded".to_string()),
                    ("failed".to_string(), "failed".to_string()),
                ]),
                evidence_required: BTreeMap::new(),
                recovery_targets: Vec::new(),
                intake: None,
            },
            &BTreeMap::from([("run".to_string(), WorkflowActivityPolicy::default())]),
        )?;
        let expected_version = i64::from(definition.definition_version());
        let expected_hash = definition.definition_hash().to_string();
        let mut registry = WorkflowDefinitionRegistry::with_builtins();
        registry.register_declarative_current(definition)?;

        let rows = terminal_state_selector_rows(&registry);
        let pinned_rows = rows
            .definition_ids
            .iter()
            .zip(&rows.definition_versions)
            .zip(&rows.definition_hashes)
            .zip(&rows.states)
            .filter(|(((id, _), _), _)| id.as_str() == definition_id)
            .collect::<Vec<_>>();
        assert_eq!(pinned_rows.len(), 3);
        assert!(pinned_rows.iter().all(|(((id, version), hash), _)| {
            id.as_str() == definition_id
                && **version == Some(expected_version)
                && hash.as_deref() == Some(expected_hash.as_str())
        }));

        let (unversioned_ids, _, _) = terminal_task_status_rows(&registry);
        assert!(!unversioned_ids.iter().any(|id| id == definition_id));
        Ok(())
    }
}
