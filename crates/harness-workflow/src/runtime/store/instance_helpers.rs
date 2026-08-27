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

pub(super) struct ProgressStateSelectorRows {
    pub(super) definition_ids: Vec<String>,
    pub(super) definition_versions: Vec<Option<i64>>,
    pub(super) definition_hashes: Vec<Option<String>>,
    pub(super) states: Vec<String>,
}

impl ProgressStateSelectorRows {
    pub(super) fn insert(
        &mut self,
        definition_id: String,
        definition_version: Option<i64>,
        definition_hash: Option<String>,
        state: String,
    ) {
        let duplicate = self
            .definition_ids
            .iter()
            .zip(&self.definition_versions)
            .zip(&self.definition_hashes)
            .zip(&self.states)
            .any(
                |(((registered_id, registered_version), registered_hash), registered_state)| {
                    registered_id == &definition_id
                        && registered_version == &definition_version
                        && registered_hash == &definition_hash
                        && registered_state == &state
                },
            );
        if duplicate {
            return;
        }
        self.definition_ids.push(definition_id);
        self.definition_versions.push(definition_version);
        self.definition_hashes.push(definition_hash);
        self.states.push(state);
    }
}

pub(super) fn progress_state_selector_rows(
    registry: &WorkflowDefinitionRegistry,
    progress_mode: crate::runtime::WorkflowProgressMode,
) -> ProgressStateSelectorRows {
    let mut rows = ProgressStateSelectorRows {
        definition_ids: Vec::new(),
        definition_versions: Vec::new(),
        definition_hashes: Vec::new(),
        states: Vec::new(),
    };
    for definition_id in registry.known_definition_ids() {
        for selector in registry.progress_state_selectors(&definition_id, progress_mode) {
            rows.insert(
                definition_id.clone(),
                selector.definition_version.map(i64::from),
                selector.definition_hash,
                selector.state,
            );
        }
    }
    rows
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

pub(super) struct TerminalTaskStatusRows {
    pub(super) definition_ids: Vec<String>,
    pub(super) states: Vec<String>,
    pub(super) task_statuses: Vec<String>,
    pub(super) definition_versions: Vec<Option<i64>>,
    pub(super) definition_hashes: Vec<Option<String>>,
}

pub(super) fn terminal_task_status_rows(
    registry: &WorkflowDefinitionRegistry,
) -> TerminalTaskStatusRows {
    let mut rows = TerminalTaskStatusRows {
        definition_ids: Vec::new(),
        states: Vec::new(),
        task_statuses: Vec::new(),
        definition_versions: Vec::new(),
        definition_hashes: Vec::new(),
    };
    for definition_id in registry.known_definition_ids() {
        for selector in registry.terminal_state_selectors(&definition_id) {
            rows.definition_ids.push(definition_id.clone());
            rows.states.push(selector.state);
            rows.task_statuses.push(
                match selector.terminal_state {
                    WorkflowTerminalState::Succeeded => "done",
                    WorkflowTerminalState::Failed => "failed",
                    WorkflowTerminalState::Cancelled => "cancelled",
                }
                .to_string(),
            );
            rows.definition_versions
                .push(selector.definition_version.map(i64::from));
            rows.definition_hashes.push(selector.definition_hash);
        }
    }
    rows
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

        let task_rows = terminal_task_status_rows(&registry);
        let task_pins = task_rows
            .definition_ids
            .iter()
            .zip(&task_rows.definition_versions)
            .zip(&task_rows.definition_hashes)
            .filter(|((id, _), _)| id.as_str() == definition_id)
            .collect::<Vec<_>>();
        assert_eq!(task_pins.len(), 3);
        assert!(task_pins.iter().all(|((_, version), hash)| {
            **version == Some(expected_version) && hash.as_deref() == Some(expected_hash.as_str())
        }));
        Ok(())
    }

    #[test]
    fn github_terminal_task_rows_distinguish_legacy_and_current_pins() {
        let rows = terminal_task_status_rows(&WorkflowDefinitionRegistry::with_builtins());
        let github_rows = rows
            .definition_ids
            .iter()
            .zip(&rows.definition_versions)
            .zip(&rows.definition_hashes)
            .filter(|((id, _), _)| id.as_str() == "github_issue_pr")
            .collect::<Vec<_>>();

        assert!(github_rows
            .iter()
            .any(|((_, version), hash)| **version == Some(1) && hash.is_none()));
        assert!(github_rows.iter().any(|((_, version), hash)| {
            **version
                == Some(i64::from(
                    crate::runtime::GITHUB_ISSUE_PR_DEFINITION_VERSION,
                ))
                && hash.as_deref()
                    == Some(crate::runtime::github_issue_pr_definition_hash().as_str())
        }));
        assert!(github_rows
            .iter()
            .all(|((_, version), _)| version.is_some()));
    }

    #[test]
    fn github_progress_rows_separate_legacy_version_from_current_pin() {
        let rows = progress_state_selector_rows(
            &WorkflowDefinitionRegistry::with_builtins(),
            crate::runtime::WorkflowProgressMode::CommandDriven,
        );
        let github_rows = rows
            .definition_ids
            .iter()
            .zip(&rows.definition_versions)
            .zip(&rows.definition_hashes)
            .zip(&rows.states)
            .filter(|(((id, _), _), _)| id.as_str() == "github_issue_pr")
            .collect::<Vec<_>>();

        assert!(github_rows
            .iter()
            .any(|(((_, version), hash), _)| **version == Some(1) && hash.is_none()));
        assert!(github_rows.iter().any(|(((_, version), hash), _)| {
            **version
                == Some(i64::from(
                    crate::runtime::GITHUB_ISSUE_PR_DEFINITION_VERSION,
                ))
                && hash.as_deref()
                    == Some(crate::runtime::github_issue_pr_definition_hash().as_str())
        }));
        assert!(github_rows
            .iter()
            .all(|(((_, version), _), _)| version.is_some()));
    }
}
