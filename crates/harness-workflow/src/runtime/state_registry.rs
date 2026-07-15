use super::{
    pr_feedback::PR_FEEDBACK_DEFINITION_ID,
    prompt_task::PROMPT_TASK_DEFINITION_ID,
    quality_gate::QUALITY_GATE_DEFINITION_ID,
    reducer::GITHUB_ISSUE_PR_DEFINITION_ID,
    validator::{DecisionValidator, TransitionAllowlist},
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, OnceLock, RwLock};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowTerminalState {
    Succeeded,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowProgressMode {
    CommandDriven,
    ExternalWait,
    OperatorGate,
    ParentHandoff,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct WorkflowStateKey {
    pub definition_id: Arc<str>,
    pub state: Arc<str>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowStateDefinition {
    pub key: WorkflowStateKey,
    pub progress_mode: Option<WorkflowProgressMode>,
    pub terminal_state: Option<WorkflowTerminalState>,
}

impl WorkflowStateDefinition {
    pub fn active(
        definition_id: impl Into<Arc<str>>,
        state: impl Into<Arc<str>>,
        progress_mode: WorkflowProgressMode,
    ) -> Self {
        Self {
            key: WorkflowStateKey {
                definition_id: definition_id.into(),
                state: state.into(),
            },
            progress_mode: Some(progress_mode),
            terminal_state: None,
        }
    }

    pub fn terminal(
        definition_id: impl Into<Arc<str>>,
        state: impl Into<Arc<str>>,
        terminal_state: WorkflowTerminalState,
    ) -> Self {
        Self {
            key: WorkflowStateKey {
                definition_id: definition_id.into(),
                state: state.into(),
            },
            progress_mode: None,
            terminal_state: Some(terminal_state),
        }
    }

    fn has_complete_progress_contract(&self) -> bool {
        self.progress_mode.is_some() != self.terminal_state.is_some()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegisteredWorkflowDefinition {
    pub id: String,
    pub states: Vec<WorkflowStateDefinition>,
    pub allowlist: TransitionAllowlist,
}

impl RegisteredWorkflowDefinition {
    pub fn new(
        id: impl Into<String>,
        states: Vec<WorkflowStateDefinition>,
        allowlist: TransitionAllowlist,
    ) -> Self {
        Self {
            id: id.into(),
            states,
            allowlist,
        }
    }
}

#[derive(Debug)]
pub struct WorkflowDefinitionRegistry {
    definitions: HashMap<String, Arc<RegisteredWorkflowDefinition>>,
    definition_ids: Vec<String>,
    frozen: bool,
}

impl WorkflowDefinitionRegistry {
    pub fn new() -> Self {
        Self {
            definitions: HashMap::new(),
            definition_ids: Vec::new(),
            frozen: false,
        }
    }

    fn with_builtins() -> Self {
        let mut registry = Self::new();
        for definition in builtin_definitions() {
            registry
                .register(definition)
                .expect("built-in workflow definitions must be unique");
        }
        registry
    }

    #[cfg(test)]
    pub fn new_for_tests() -> Self {
        Self::new()
    }

    pub fn register(&mut self, definition: RegisteredWorkflowDefinition) -> anyhow::Result<()> {
        if self.frozen {
            anyhow::bail!(
                "workflow definition registry is frozen; cannot register '{}'",
                definition.id
            );
        }
        if self.definitions.contains_key(&definition.id) {
            anyhow::bail!(
                "workflow definition '{}' is already registered",
                definition.id
            );
        }
        if let Some(state) = definition
            .states
            .iter()
            .find(|state| !state.has_complete_progress_contract())
        {
            anyhow::bail!(
                "workflow definition '{}' state '{}' must declare exactly one of progress_mode or terminal_state",
                definition.id,
                state.key.state
            );
        }
        self.definition_ids.push(definition.id.clone());
        self.definitions
            .insert(definition.id.clone(), Arc::new(definition));
        Ok(())
    }

    pub fn freeze(&mut self) {
        self.frozen = true;
    }

    pub fn is_frozen(&self) -> bool {
        self.frozen
    }

    pub fn definition(&self, definition_id: &str) -> Option<Arc<RegisteredWorkflowDefinition>> {
        self.definitions.get(definition_id).cloned()
    }

    pub fn decision_validator_for_definition(
        &self,
        definition_id: &str,
    ) -> Option<DecisionValidator> {
        self.definition(definition_id).map(|definition| {
            DecisionValidator::for_definition(definition_id, definition.allowlist.clone())
        })
    }

    pub fn known_definition_ids(&self) -> Vec<String> {
        self.definition_ids.clone()
    }
}

impl Default for WorkflowDefinitionRegistry {
    fn default() -> Self {
        Self::new()
    }
}

static REGISTRY: OnceLock<RwLock<WorkflowDefinitionRegistry>> = OnceLock::new();

fn registry() -> &'static RwLock<WorkflowDefinitionRegistry> {
    REGISTRY.get_or_init(|| RwLock::new(WorkflowDefinitionRegistry::with_builtins()))
}

pub fn register_workflow_definition(
    definition: RegisteredWorkflowDefinition,
) -> anyhow::Result<()> {
    registry()
        .write()
        .expect("workflow definition registry lock poisoned")
        .register(definition)
}

pub fn freeze_workflow_definition_registry() {
    registry()
        .write()
        .expect("workflow definition registry lock poisoned")
        .freeze();
}

pub fn workflow_definition(definition_id: &str) -> Option<Arc<RegisteredWorkflowDefinition>> {
    registry()
        .read()
        .expect("workflow definition registry lock poisoned")
        .definition(definition_id)
}

pub fn decision_validator_for_definition(definition_id: &str) -> Option<DecisionValidator> {
    registry()
        .read()
        .expect("workflow definition registry lock poisoned")
        .decision_validator_for_definition(definition_id)
}

pub fn known_workflow_definition_ids() -> Vec<String> {
    registry()
        .read()
        .expect("workflow definition registry lock poisoned")
        .known_definition_ids()
}

pub fn workflow_states_for_definition(definition_id: &str) -> Vec<WorkflowStateDefinition> {
    workflow_definition(definition_id)
        .map(|definition| definition.states.clone())
        .unwrap_or_default()
}

pub fn workflow_terminal_state_names_for_definition(definition_id: &str) -> Vec<String> {
    workflow_definition(definition_id)
        .map(|definition| {
            definition
                .states
                .iter()
                .filter(|state| state.terminal_state.is_some())
                .map(|state| state.key.state.to_string())
                .collect()
        })
        .unwrap_or_default()
}

pub fn workflow_state_definition(
    definition_id: &str,
    state: &str,
) -> Option<WorkflowStateDefinition> {
    workflow_definition(definition_id).and_then(|definition| {
        definition
            .states
            .iter()
            .find(|definition| definition.key.state.as_ref() == state)
            .cloned()
    })
}

pub fn workflow_state_exists(definition_id: &str, state: &str) -> bool {
    workflow_state_definition(definition_id, state).is_some()
}

pub fn workflow_state_terminal_state(
    definition_id: &str,
    state: &str,
) -> Option<WorkflowTerminalState> {
    workflow_state_definition(definition_id, state)?.terminal_state
}

pub fn workflow_state_progress_mode(
    definition_id: &str,
    state: &str,
) -> Option<WorkflowProgressMode> {
    workflow_state_definition(definition_id, state)?.progress_mode
}

fn builtin_definitions() -> [RegisteredWorkflowDefinition; 4] {
    [
        github_issue_pr_definition(),
        prompt_task_definition(),
        quality_gate_definition(),
        pr_feedback_definition(),
    ]
}

fn github_issue_pr_definition() -> RegisteredWorkflowDefinition {
    use WorkflowProgressMode::{CommandDriven, ExternalWait, OperatorGate, ParentHandoff};

    definition(
        GITHUB_ISSUE_PR_DEFINITION_ID,
        vec![
            active(GITHUB_ISSUE_PR_DEFINITION_ID, "discovered", CommandDriven),
            active(
                GITHUB_ISSUE_PR_DEFINITION_ID,
                "awaiting_dependencies",
                ExternalWait,
            ),
            active(GITHUB_ISSUE_PR_DEFINITION_ID, "scheduled", CommandDriven),
            active(GITHUB_ISSUE_PR_DEFINITION_ID, "planning", CommandDriven),
            active(GITHUB_ISSUE_PR_DEFINITION_ID, "implementing", CommandDriven),
            active(GITHUB_ISSUE_PR_DEFINITION_ID, "replanning", CommandDriven),
            active(GITHUB_ISSUE_PR_DEFINITION_ID, "pr_open", ExternalWait),
            active(
                GITHUB_ISSUE_PR_DEFINITION_ID,
                "local_review_gate",
                CommandDriven,
            ),
            active(
                GITHUB_ISSUE_PR_DEFINITION_ID,
                "awaiting_feedback",
                ExternalWait,
            ),
            active(
                GITHUB_ISSUE_PR_DEFINITION_ID,
                "addressing_feedback",
                CommandDriven,
            ),
            active(
                GITHUB_ISSUE_PR_DEFINITION_ID,
                "quality_gate_pending",
                ParentHandoff,
            ),
            active(
                GITHUB_ISSUE_PR_DEFINITION_ID,
                "ready_to_merge",
                OperatorGate,
            ),
            active(GITHUB_ISSUE_PR_DEFINITION_ID, "merging", CommandDriven),
            active(GITHUB_ISSUE_PR_DEFINITION_ID, "blocked", OperatorGate),
            terminal(
                GITHUB_ISSUE_PR_DEFINITION_ID,
                "done",
                WorkflowTerminalState::Succeeded,
            ),
            terminal(
                GITHUB_ISSUE_PR_DEFINITION_ID,
                "failed",
                WorkflowTerminalState::Failed,
            ),
            terminal(
                GITHUB_ISSUE_PR_DEFINITION_ID,
                "cancelled",
                WorkflowTerminalState::Cancelled,
            ),
        ],
        TransitionAllowlist::github_issue_pr_defaults(),
    )
}

fn prompt_task_definition() -> RegisteredWorkflowDefinition {
    use WorkflowProgressMode::{CommandDriven, ExternalWait, OperatorGate};

    definition(
        PROMPT_TASK_DEFINITION_ID,
        vec![
            active(PROMPT_TASK_DEFINITION_ID, "submitted", CommandDriven),
            active(
                PROMPT_TASK_DEFINITION_ID,
                "awaiting_dependencies",
                ExternalWait,
            ),
            active(PROMPT_TASK_DEFINITION_ID, "implementing", CommandDriven),
            active(PROMPT_TASK_DEFINITION_ID, "blocked", OperatorGate),
            terminal(
                PROMPT_TASK_DEFINITION_ID,
                "done",
                WorkflowTerminalState::Succeeded,
            ),
            terminal(
                PROMPT_TASK_DEFINITION_ID,
                "failed",
                WorkflowTerminalState::Failed,
            ),
            terminal(
                PROMPT_TASK_DEFINITION_ID,
                "cancelled",
                WorkflowTerminalState::Cancelled,
            ),
        ],
        TransitionAllowlist::prompt_task_defaults(),
    )
}

fn quality_gate_definition() -> RegisteredWorkflowDefinition {
    use WorkflowProgressMode::{CommandDriven, OperatorGate};

    definition(
        QUALITY_GATE_DEFINITION_ID,
        vec![
            active(QUALITY_GATE_DEFINITION_ID, "pending", CommandDriven),
            active(QUALITY_GATE_DEFINITION_ID, "checking", CommandDriven),
            active(QUALITY_GATE_DEFINITION_ID, "blocked", OperatorGate),
            terminal(
                QUALITY_GATE_DEFINITION_ID,
                "passed",
                WorkflowTerminalState::Succeeded,
            ),
            terminal(
                QUALITY_GATE_DEFINITION_ID,
                "failed",
                WorkflowTerminalState::Failed,
            ),
            terminal(
                QUALITY_GATE_DEFINITION_ID,
                "cancelled",
                WorkflowTerminalState::Cancelled,
            ),
        ],
        TransitionAllowlist::quality_gate_defaults(),
    )
}

fn pr_feedback_definition() -> RegisteredWorkflowDefinition {
    use WorkflowProgressMode::{CommandDriven, OperatorGate, ParentHandoff};

    definition(
        PR_FEEDBACK_DEFINITION_ID,
        vec![
            active(PR_FEEDBACK_DEFINITION_ID, "pending", CommandDriven),
            active(PR_FEEDBACK_DEFINITION_ID, "inspecting", CommandDriven),
            active(PR_FEEDBACK_DEFINITION_ID, "feedback_found", ParentHandoff),
            active(
                PR_FEEDBACK_DEFINITION_ID,
                "no_actionable_feedback",
                ParentHandoff,
            ),
            active(PR_FEEDBACK_DEFINITION_ID, "ready_to_merge", ParentHandoff),
            active(PR_FEEDBACK_DEFINITION_ID, "blocked", OperatorGate),
            terminal(
                PR_FEEDBACK_DEFINITION_ID,
                "done",
                WorkflowTerminalState::Succeeded,
            ),
            terminal(
                PR_FEEDBACK_DEFINITION_ID,
                "failed",
                WorkflowTerminalState::Failed,
            ),
            terminal(
                PR_FEEDBACK_DEFINITION_ID,
                "cancelled",
                WorkflowTerminalState::Cancelled,
            ),
        ],
        TransitionAllowlist::pr_feedback_defaults(),
    )
}

fn definition(
    id: &'static str,
    states: Vec<WorkflowStateDefinition>,
    allowlist: TransitionAllowlist,
) -> RegisteredWorkflowDefinition {
    RegisteredWorkflowDefinition::new(id, states, allowlist)
}

fn active(
    definition_id: &'static str,
    state: &'static str,
    progress_mode: WorkflowProgressMode,
) -> WorkflowStateDefinition {
    WorkflowStateDefinition::active(definition_id, state, progress_mode)
}

fn terminal(
    definition_id: &'static str,
    state: &'static str,
    terminal_state: WorkflowTerminalState,
) -> WorkflowStateDefinition {
    WorkflowStateDefinition::terminal(definition_id, state, terminal_state)
}

#[cfg(test)]
#[path = "state_registry_equivalence_tests.rs"]
mod equivalence_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::validator::TransitionAllowlist;

    #[test]
    fn registry_scopes_success_states_to_workflow_definitions() {
        assert_eq!(
            workflow_state_terminal_state(QUALITY_GATE_DEFINITION_ID, "passed"),
            Some(WorkflowTerminalState::Succeeded)
        );
        assert_eq!(
            workflow_state_terminal_state(GITHUB_ISSUE_PR_DEFINITION_ID, "passed"),
            None
        );
        assert_eq!(
            workflow_state_terminal_state(QUALITY_GATE_DEFINITION_ID, "done"),
            None
        );
    }

    #[test]
    fn registry_lists_only_known_definition_states() {
        assert_eq!(
            known_workflow_definition_ids(),
            vec![
                GITHUB_ISSUE_PR_DEFINITION_ID,
                PROMPT_TASK_DEFINITION_ID,
                QUALITY_GATE_DEFINITION_ID,
                PR_FEEDBACK_DEFINITION_ID,
            ]
        );
        assert!(workflow_states_for_definition("unknown_workflow").is_empty());
        assert!(workflow_state_exists(
            GITHUB_ISSUE_PR_DEFINITION_ID,
            "awaiting_feedback"
        ));
        assert!(!workflow_state_exists(
            GITHUB_ISSUE_PR_DEFINITION_ID,
            "inspecting"
        ));
    }

    #[test]
    fn registry_lists_terminal_state_names_by_definition() {
        assert_eq!(
            workflow_terminal_state_names_for_definition(GITHUB_ISSUE_PR_DEFINITION_ID),
            vec!["done", "failed", "cancelled"]
        );
        assert_eq!(
            workflow_terminal_state_names_for_definition(QUALITY_GATE_DEFINITION_ID),
            vec!["passed", "failed", "cancelled"]
        );
        assert!(workflow_terminal_state_names_for_definition("unknown_workflow").is_empty());
    }

    #[test]
    fn registry_covers_validator_transition_states() {
        let allowlists = [
            (
                GITHUB_ISSUE_PR_DEFINITION_ID,
                TransitionAllowlist::github_issue_pr_defaults(),
            ),
            (
                PROMPT_TASK_DEFINITION_ID,
                TransitionAllowlist::prompt_task_defaults(),
            ),
            (
                QUALITY_GATE_DEFINITION_ID,
                TransitionAllowlist::quality_gate_defaults(),
            ),
            (
                PR_FEEDBACK_DEFINITION_ID,
                TransitionAllowlist::pr_feedback_defaults(),
            ),
        ];

        for (definition_id, allowlist) in allowlists {
            let definition = workflow_definition(definition_id)
                .expect("built-in workflow definition should be registered");
            for state in &definition.states {
                assert!(
                    state.has_complete_progress_contract(),
                    "{definition_id}.{} must declare exactly one progress or terminal contract",
                    state.key.state
                );
            }
            for rule in allowlist.rules() {
                if let Some(from_state) = rule.from_state.as_deref() {
                    assert!(
                        workflow_state_exists(definition_id, from_state),
                        "{definition_id} missing from_state {from_state}"
                    );
                }
                assert!(
                    workflow_state_definition(definition_id, &rule.to_state)
                        .is_some_and(|state| state.has_complete_progress_contract()),
                    "{definition_id} to_state {} is missing a complete contract",
                    rule.to_state,
                );
            }
        }
    }

    #[test]
    fn progress_mode_lookup_fails_closed_for_terminal_and_unknown_states() {
        assert_eq!(
            workflow_state_progress_mode(GITHUB_ISSUE_PR_DEFINITION_ID, "implementing"),
            Some(WorkflowProgressMode::CommandDriven)
        );
        assert_eq!(
            workflow_state_progress_mode(GITHUB_ISSUE_PR_DEFINITION_ID, "done"),
            None
        );
        assert_eq!(
            workflow_state_progress_mode(GITHUB_ISSUE_PR_DEFINITION_ID, "unknown"),
            None
        );
        assert_eq!(
            workflow_state_progress_mode("unknown_definition", "implementing"),
            None
        );
    }

    #[test]
    fn state_key_clones_reuse_owned_string_allocations() {
        let state = workflow_state_definition(PROMPT_TASK_DEFINITION_ID, "implementing")
            .expect("prompt task implementing state should exist");
        let cloned = state.key.clone();

        assert!(Arc::ptr_eq(&state.key.definition_id, &cloned.definition_id));
        assert!(Arc::ptr_eq(&state.key.state, &cloned.state));
    }

    #[test]
    fn duplicate_registration_fails_without_replacing_the_first_definition() {
        let mut registry = WorkflowDefinitionRegistry::new_for_tests();
        let first = definition(
            "fixture",
            vec![active(
                "fixture",
                "pending",
                WorkflowProgressMode::ExternalWait,
            )],
            TransitionAllowlist::default(),
        );
        let duplicate = definition(
            "fixture",
            vec![active(
                "fixture",
                "other",
                WorkflowProgressMode::ExternalWait,
            )],
            TransitionAllowlist::default(),
        );

        registry
            .register(first)
            .expect("first registration should pass");
        let error = registry
            .register(duplicate)
            .expect_err("duplicate registration should fail");

        assert!(error.to_string().contains("already registered"));
        assert!(registry
            .definition("fixture")
            .is_some_and(|definition| definition.states[0].key.state.as_ref() == "pending"));
    }

    #[test]
    fn freeze_is_idempotent_and_rejects_late_registration() {
        let mut registry = WorkflowDefinitionRegistry::new_for_tests();
        registry.freeze();
        registry.freeze();

        let error = registry
            .register(definition(
                "late",
                vec![active(
                    "late",
                    "pending",
                    WorkflowProgressMode::ExternalWait,
                )],
                TransitionAllowlist::default(),
            ))
            .expect_err("post-freeze registration should fail");

        assert!(registry.is_frozen());
        assert!(error.to_string().contains("is frozen"));
        assert!(registry.definition("late").is_none());
    }

    #[test]
    fn registration_rejects_incomplete_or_conflicting_progress_contracts() {
        let invalid_states = [
            WorkflowStateDefinition {
                key: WorkflowStateKey {
                    definition_id: Arc::from("fixture"),
                    state: Arc::from("missing"),
                },
                progress_mode: None,
                terminal_state: None,
            },
            WorkflowStateDefinition {
                key: WorkflowStateKey {
                    definition_id: Arc::from("fixture"),
                    state: Arc::from("conflicting"),
                },
                progress_mode: Some(WorkflowProgressMode::OperatorGate),
                terminal_state: Some(WorkflowTerminalState::Failed),
            },
        ];

        for state in invalid_states {
            let mut registry = WorkflowDefinitionRegistry::new_for_tests();
            let error = registry
                .register(RegisteredWorkflowDefinition::new(
                    "fixture",
                    vec![state],
                    TransitionAllowlist::default(),
                ))
                .expect_err("invalid state contract should fail registration");

            assert!(error.to_string().contains("exactly one"));
            assert!(registry.definition("fixture").is_none());
        }
    }
}
