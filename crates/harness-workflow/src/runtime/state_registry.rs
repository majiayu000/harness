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
use std::sync::Arc;

mod builtins;
mod evidence_policy;
mod versioning;

use self::builtins::{builtin_definitions, builtin_registered_definitions};

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct WorkflowTerminalStateSelector {
    pub definition_version: Option<u32>,
    pub definition_hash: Option<String>,
    pub state: String,
    pub terminal_state: WorkflowTerminalState,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct WorkflowProgressStateSelector {
    pub definition_version: Option<u32>,
    pub definition_hash: Option<String>,
    pub state: String,
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

    pub fn with_builtins() -> Self {
        let mut registry = Self::new();
        if let Err(error) = registry.register_declarative_current_batch(builtin_definitions()) {
            panic!("built-in workflow definitions must be unique and valid: {error}");
        }
        if let Err(error) = registry
            .register_declarative_historical_batch([versioning::github_issue_pr_v1_definition()])
        {
            panic!("historical built-in workflow definitions must be unique and valid: {error}");
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

    /// Freeze this registry and return the immutable handle shared by a
    /// workflow runtime. Runtime lookups never acquire a blocking lock.
    pub fn into_shared(mut self) -> Arc<Self> {
        self.freeze();
        Arc::new(self)
    }

    pub fn definition(&self, definition_id: &str) -> Option<Arc<RegisteredWorkflowDefinition>> {
        self.definitions.get(definition_id).cloned()
    }

    pub fn decision_validator_for_definition(
        &self,
        definition_id: &str,
    ) -> Option<DecisionValidator> {
        self.definition(definition_id).map(|definition| {
            DecisionValidator::for_definition(
                definition_id,
                definition.allowlist.clone(),
                definition.states.clone(),
            )
        })
    }

    pub fn decision_validator_for_instance(
        &self,
        instance: &WorkflowInstance,
    ) -> Result<Option<DecisionValidator>, DeclarativeDefinitionPinError> {
        if is_builtin_definition_id(&instance.definition_id)
            && instance.definition_id != GITHUB_ISSUE_PR_DEFINITION_ID
        {
            return Ok(self.decision_validator_for_definition(&instance.definition_id));
        }
        match self.resolve_declarative_definition(instance) {
            // Carry the exact version and content hash the pin resolved to, so
            // the store can re-verify at commit that this validator still
            // governs the row it loaded (GH-1864).
            DeclarativeDefinitionResolution::Resolved(definition) => {
                Ok(Some(DecisionValidator::for_declarative_definition(
                    &instance.definition_id,
                    definition.definition_version(),
                    definition.definition_hash(),
                    definition.registered().allowlist.clone(),
                    definition.registered().states.clone(),
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

    pub fn declarative_definition_for_instance(
        &self,
        instance: &WorkflowInstance,
    ) -> Option<Arc<DeclarativeWorkflowDefinition>> {
        match self.resolve_declarative_definition(instance) {
            DeclarativeDefinitionResolution::Resolved(definition) => Some(definition),
            DeclarativeDefinitionResolution::NotDeclarative
            | DeclarativeDefinitionResolution::PinError(_) => None,
        }
    }

    pub fn instance_has_classifier_activity(
        &self,
        instance: &WorkflowInstance,
        activity: &str,
    ) -> Result<bool, DeclarativeDefinitionPinError> {
        match self.resolve_declarative_definition(instance) {
            DeclarativeDefinitionResolution::Resolved(definition) => {
                Ok(definition.classifier_activities().contains(activity))
            }
            DeclarativeDefinitionResolution::PinError(error) => Err(error),
            DeclarativeDefinitionResolution::NotDeclarative => Ok(false),
        }
    }

    pub fn instance_is_declarative(&self, instance: &WorkflowInstance) -> bool {
        !matches!(
            self.resolve_declarative_definition(instance),
            DeclarativeDefinitionResolution::NotDeclarative
        )
    }

    pub fn states_for_definition(&self, definition_id: &str) -> Vec<WorkflowStateDefinition> {
        self.definition(definition_id)
            .map(|definition| definition.states.clone())
            .unwrap_or_default()
    }

    pub fn terminal_state_names_for_definition(&self, definition_id: &str) -> Vec<String> {
        self.definition(definition_id)
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

    pub fn state_definition(
        &self,
        definition_id: &str,
        state: &str,
    ) -> Option<WorkflowStateDefinition> {
        self.definition(definition_id).and_then(|definition| {
            definition
                .states
                .iter()
                .find(|definition| definition.key.state.as_ref() == state)
                .cloned()
        })
    }

    pub fn state_exists(&self, definition_id: &str, state: &str) -> bool {
        self.state_definition(definition_id, state).is_some()
    }

    pub fn state_terminal_state(
        &self,
        definition_id: &str,
        state: &str,
    ) -> Option<WorkflowTerminalState> {
        self.state_definition(definition_id, state)?.terminal_state
    }

    pub fn state_progress_mode(
        &self,
        definition_id: &str,
        state: &str,
    ) -> Option<WorkflowProgressMode> {
        self.state_definition(definition_id, state)?.progress_mode
    }

    pub fn state_progress_mode_for_version(
        &self,
        definition_id: &str,
        definition_version: u32,
        state: &str,
    ) -> Option<WorkflowProgressMode> {
        self.state_definition_for_version(definition_id, definition_version, state)?
            .progress_mode
    }

    pub fn state_terminal_state_for_version(
        &self,
        definition_id: &str,
        definition_version: u32,
        state: &str,
    ) -> Option<WorkflowTerminalState> {
        self.state_definition_for_version(definition_id, definition_version, state)?
            .terminal_state
    }

    pub fn terminal_state_for_instance(
        &self,
        instance: &WorkflowInstance,
    ) -> Option<WorkflowTerminalState> {
        self.state_definition_for_instance(instance, &instance.state)?
            .terminal_state
    }

    pub fn instance_is_terminal(&self, instance: &WorkflowInstance) -> bool {
        self.terminal_state_for_instance(instance).is_some()
    }

    pub(super) fn terminal_state_selectors(
        &self,
        definition_id: &str,
    ) -> Vec<WorkflowTerminalStateSelector> {
        let mut selectors = self
            .declarative_versions
            .iter()
            .filter(|((registered_id, _), _)| registered_id == definition_id)
            .flat_map(|((_, definition_version), definition)| {
                definition.registered().states.iter().filter_map(|state| {
                    let unpinned_legacy_builtin =
                        is_builtin_definition_id(definition_id) && *definition_version == 1;
                    Some(WorkflowTerminalStateSelector {
                        definition_version: (!unpinned_legacy_builtin)
                            .then_some(*definition_version),
                        definition_hash: (!unpinned_legacy_builtin)
                            .then(|| definition.definition_hash().to_string()),
                        state: state.key.state.to_string(),
                        terminal_state: state.terminal_state?,
                    })
                })
            })
            .collect::<Vec<_>>();
        if selectors.is_empty() {
            if let Some(definition) = self.definition(definition_id) {
                selectors.extend(definition.states.iter().filter_map(|state| {
                    Some(WorkflowTerminalStateSelector {
                        definition_version: None,
                        definition_hash: None,
                        state: state.key.state.to_string(),
                        terminal_state: state.terminal_state?,
                    })
                }));
            }
        }
        selectors.sort_by(|left, right| {
            left.definition_version
                .cmp(&right.definition_version)
                .then_with(|| left.state.cmp(&right.state))
        });
        selectors
    }

    pub(super) fn progress_state_selectors(
        &self,
        definition_id: &str,
        progress_mode: WorkflowProgressMode,
    ) -> Vec<WorkflowProgressStateSelector> {
        let mut selectors = self
            .declarative_versions
            .iter()
            .filter(|((registered_id, _), _)| registered_id == definition_id)
            .flat_map(|((_, definition_version), definition)| {
                definition
                    .registered()
                    .states
                    .iter()
                    .filter(|state| state.progress_mode == Some(progress_mode))
                    .map(|state| {
                        let unpinned_legacy_builtin =
                            is_builtin_definition_id(definition_id) && *definition_version == 1;
                        WorkflowProgressStateSelector {
                            definition_version: (!unpinned_legacy_builtin)
                                .then_some(*definition_version),
                            definition_hash: (!unpinned_legacy_builtin)
                                .then(|| definition.definition_hash().to_string()),
                            state: state.key.state.to_string(),
                        }
                    })
            })
            .collect::<Vec<_>>();
        if selectors.is_empty() {
            if let Some(definition) = self.definition(definition_id) {
                selectors.extend(
                    definition
                        .states
                        .iter()
                        .filter(|state| state.progress_mode == Some(progress_mode))
                        .map(|state| WorkflowProgressStateSelector {
                            definition_version: None,
                            definition_hash: None,
                            state: state.key.state.to_string(),
                        }),
                );
            }
        }
        selectors.sort_by(|left, right| {
            left.definition_version
                .cmp(&right.definition_version)
                .then_with(|| left.state.cmp(&right.state))
        });
        selectors
    }
}

impl Default for WorkflowDefinitionRegistry {
    fn default() -> Self {
        Self::new()
    }
}

fn is_builtin_definition_id(definition_id: &str) -> bool {
    [
        GITHUB_ISSUE_PR_DEFINITION_ID,
        PROMPT_TASK_DEFINITION_ID,
        QUALITY_GATE_DEFINITION_ID,
        PR_FEEDBACK_DEFINITION_ID,
    ]
    .contains(&definition_id)
}
