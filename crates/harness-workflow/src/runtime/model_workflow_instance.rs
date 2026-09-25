use super::model::{WorkflowInstance, WorkflowLease, WorkflowSubject};
use super::state_registry::{WorkflowDefinitionRegistry, WorkflowTerminalState};
use chrono::{DateTime, Utc};
use serde_json::Value;
use uuid::Uuid;

impl WorkflowInstance {
    pub fn new(
        definition_id: impl Into<String>,
        definition_version: u32,
        state: impl Into<String>,
        subject: WorkflowSubject,
    ) -> Self {
        let now = Utc::now();
        Self {
            id: Uuid::new_v4().to_string(),
            definition_id: definition_id.into(),
            definition_version,
            state: state.into(),
            subject,
            parent_workflow_id: None,
            data: Value::Object(Default::default()),
            data_provenance: Some(Default::default()),
            version: 0,
            lease: None,
            created_at: now,
            updated_at: now,
        }
    }

    pub fn is_terminal(&self) -> bool {
        WorkflowDefinitionRegistry::with_builtins().instance_is_terminal(self)
    }

    pub fn is_terminal_with_registry(&self, registry: &WorkflowDefinitionRegistry) -> bool {
        self.terminal_state_with_registry(registry).is_some()
    }

    pub fn terminal_state(&self) -> Option<WorkflowTerminalState> {
        WorkflowDefinitionRegistry::with_builtins().terminal_state_for_instance(self)
    }

    pub fn terminal_state_with_registry(
        &self,
        registry: &WorkflowDefinitionRegistry,
    ) -> Option<WorkflowTerminalState> {
        registry.terminal_state_for_instance(self)
    }

    pub fn with_id(mut self, id: impl Into<String>) -> Self {
        self.id = id.into();
        self
    }

    pub fn with_parent(mut self, parent_workflow_id: impl Into<String>) -> Self {
        self.parent_workflow_id = Some(parent_workflow_id.into());
        self
    }

    pub fn with_lease(mut self, owner: impl Into<String>, expires_at: DateTime<Utc>) -> Self {
        self.lease = Some(WorkflowLease {
            owner: owner.into(),
            expires_at,
        });
        self
    }
}
