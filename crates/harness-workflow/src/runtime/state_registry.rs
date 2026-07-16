use super::{
    declarative::DeclarativeWorkflowDefinition,
    model::WorkflowInstance,
    pr_feedback::PR_FEEDBACK_DEFINITION_ID,
    prompt_task::PROMPT_TASK_DEFINITION_ID,
    quality_gate::QUALITY_GATE_DEFINITION_ID,
    reducer::GITHUB_ISSUE_PR_DEFINITION_ID,
    validator::{DecisionValidator, TransitionAllowlist},
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, OnceLock, RwLock};

mod versioning;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeclarativeDefinitionPinError {
    MissingVersion,
    MissingHash,
    InvalidHash,
    HashMismatch,
}

#[derive(Debug, Clone)]
pub enum DeclarativeDefinitionResolution {
    NotDeclarative,
    Resolved(Arc<DeclarativeWorkflowDefinition>),
    PinError(DeclarativeDefinitionPinError),
}

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

#[derive(Debug, Clone)]
pub struct WorkflowDefinitionRegistry {
    definitions: HashMap<String, Arc<RegisteredWorkflowDefinition>>,
    declarative_versions: HashMap<(String, u32), Arc<DeclarativeWorkflowDefinition>>,
    current_declarative_versions: HashMap<String, u32>,
    definition_ids: Vec<String>,
    frozen: bool,
}

impl WorkflowDefinitionRegistry {
    pub fn new() -> Self {
        Self {
            definitions: HashMap::new(),
            declarative_versions: HashMap::new(),
            current_declarative_versions: HashMap::new(),
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
        self.ensure_mutable(&definition.id)?;
        if self.definitions.contains_key(&definition.id) {
            anyhow::bail!(
                "workflow definition '{}' is already registered",
                definition.id
            );
        }
        if self
            .declarative_versions
            .keys()
            .any(|(definition_id, _)| definition_id == &definition.id)
        {
            anyhow::bail!(
                "workflow definition '{}' has registered declarative history",
                definition.id
            );
        }
        Self::validate_registered_definition(&definition)?;
        self.definition_ids.push(definition.id.clone());
        self.definitions
            .insert(definition.id.clone(), Arc::new(definition));
        Ok(())
    }

    pub fn register_batch(
        &mut self,
        definitions: impl IntoIterator<Item = RegisteredWorkflowDefinition>,
    ) -> anyhow::Result<()> {
        let mut staged = self.clone();
        for definition in definitions {
            staged.register(definition)?;
        }
        *self = staged;
        Ok(())
    }

    fn ensure_mutable(&self, definition_id: &str) -> anyhow::Result<()> {
        if self.frozen {
            anyhow::bail!(
                "workflow definition registry is frozen; cannot register '{}'",
                definition_id
            );
        }
        Ok(())
    }

    fn validate_registered_definition(
        definition: &RegisteredWorkflowDefinition,
    ) -> anyhow::Result<()> {
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

    pub fn decision_validator_for_instance(
        &self,
        instance: &WorkflowInstance,
    ) -> Result<Option<DecisionValidator>, DeclarativeDefinitionPinError> {
        match self.resolve_declarative_definition(instance) {
            DeclarativeDefinitionResolution::Resolved(definition) => {
                Ok(Some(DecisionValidator::for_definition(
                    &instance.definition_id,
                    definition.registered().allowlist.clone(),
                )))
            }
            DeclarativeDefinitionResolution::PinError(error) => Err(error),
            DeclarativeDefinitionResolution::NotDeclarative => {
                Ok(self.decision_validator_for_definition(&instance.definition_id))
            }
        }
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

pub fn register_declarative_workflow_definitions(
    definitions: impl IntoIterator<Item = DeclarativeWorkflowDefinition>,
) -> anyhow::Result<()> {
    registry()
        .write()
        .expect("workflow definition registry lock poisoned")
        .register_declarative_current_batch(definitions)
}

pub fn register_historical_declarative_workflow_definitions(
    definitions: impl IntoIterator<Item = DeclarativeWorkflowDefinition>,
) -> anyhow::Result<()> {
    registry()
        .write()
        .expect("workflow definition registry lock poisoned")
        .register_declarative_historical_batch(definitions)
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

pub fn workflow_declarative_definition(
    definition_id: &str,
    definition_version: u32,
) -> Option<Arc<DeclarativeWorkflowDefinition>> {
    registry()
        .read()
        .expect("workflow definition registry lock poisoned")
        .declarative_definition(definition_id, definition_version)
}

pub fn current_declarative_workflow_definition(
    definition_id: &str,
) -> Option<Arc<DeclarativeWorkflowDefinition>> {
    registry()
        .read()
        .expect("workflow definition registry lock poisoned")
        .current_declarative_definition(definition_id)
}

pub fn workflow_definition_for_version(
    definition_id: &str,
    definition_version: u32,
) -> Option<Arc<RegisteredWorkflowDefinition>> {
    registry()
        .read()
        .expect("workflow definition registry lock poisoned")
        .definition_for_version(definition_id, definition_version)
}

pub fn decision_validator_for_definition(definition_id: &str) -> Option<DecisionValidator> {
    registry()
        .read()
        .expect("workflow definition registry lock poisoned")
        .decision_validator_for_definition(definition_id)
}

pub fn resolve_declarative_definition(
    instance: &WorkflowInstance,
) -> DeclarativeDefinitionResolution {
    registry()
        .read()
        .expect("workflow definition registry lock poisoned")
        .resolve_declarative_definition(instance)
}

pub fn decision_validator_for_instance(
    instance: &WorkflowInstance,
) -> Result<Option<DecisionValidator>, DeclarativeDefinitionPinError> {
    registry()
        .read()
        .expect("workflow definition registry lock poisoned")
        .decision_validator_for_instance(instance)
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

pub fn workflow_state_definition_for_version(
    definition_id: &str,
    definition_version: u32,
    state: &str,
) -> Option<WorkflowStateDefinition> {
    registry()
        .read()
        .expect("workflow definition registry lock poisoned")
        .state_definition_for_version(definition_id, definition_version, state)
}

pub fn workflow_state_definition_for_instance(
    instance: &WorkflowInstance,
    state: &str,
) -> Option<WorkflowStateDefinition> {
    registry()
        .read()
        .expect("workflow definition registry lock poisoned")
        .state_definition_for_instance(instance, state)
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

pub fn workflow_state_progress_mode_for_version(
    definition_id: &str,
    definition_version: u32,
    state: &str,
) -> Option<WorkflowProgressMode> {
    workflow_state_definition_for_version(definition_id, definition_version, state)?.progress_mode
}

pub fn workflow_state_terminal_state_for_version(
    definition_id: &str,
    definition_version: u32,
    state: &str,
) -> Option<WorkflowTerminalState> {
    workflow_state_definition_for_version(definition_id, definition_version, state)?.terminal_state
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
