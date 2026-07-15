use super::{
    DeclarativeWorkflowDefinition, RegisteredWorkflowDefinition, WorkflowDefinitionRegistry,
    WorkflowStateDefinition, GITHUB_ISSUE_PR_DEFINITION_ID, PROMPT_TASK_DEFINITION_ID,
    PR_FEEDBACK_DEFINITION_ID, QUALITY_GATE_DEFINITION_ID,
};
use crate::runtime::declarative_pinning::declarative_definition_identity;
use crate::runtime::model::WorkflowInstance;
use std::sync::Arc;

impl WorkflowDefinitionRegistry {
    pub fn register_declarative_current(
        &mut self,
        definition: DeclarativeWorkflowDefinition,
    ) -> anyhow::Result<()> {
        self.ensure_mutable(&definition.policy().id)?;
        if self.definitions.contains_key(&definition.policy().id) {
            anyhow::bail!(
                "workflow definition '{}' is already registered as current",
                definition.policy().id
            );
        }
        let version_key = (
            definition.policy().id.clone(),
            definition.definition_version(),
        );
        let versioned = self.checked_version_entry(version_key.clone(), definition)?;
        self.validate_declarative_definition(&versioned)?;
        self.definition_ids.push(version_key.0.clone());
        self.definitions.insert(
            version_key.0.clone(),
            Arc::new(versioned.registered().clone()),
        );
        self.current_declarative_versions
            .insert(version_key.0.clone(), version_key.1);
        self.declarative_versions.insert(version_key, versioned);
        Ok(())
    }

    pub fn register_declarative_current_batch(
        &mut self,
        definitions: impl IntoIterator<Item = DeclarativeWorkflowDefinition>,
    ) -> anyhow::Result<()> {
        let mut staged = self.clone();
        for definition in definitions {
            staged.register_declarative_current(definition)?;
        }
        *self = staged;
        Ok(())
    }

    pub fn register_declarative_historical(
        &mut self,
        definition: DeclarativeWorkflowDefinition,
    ) -> anyhow::Result<()> {
        self.ensure_mutable(&definition.policy().id)?;
        if self.definitions.contains_key(&definition.policy().id)
            && !self
                .current_declarative_versions
                .contains_key(&definition.policy().id)
        {
            anyhow::bail!(
                "historical declarative definition '{}' collides with a non-declarative current definition",
                definition.policy().id
            );
        }
        let version_key = (
            definition.policy().id.clone(),
            definition.definition_version(),
        );
        let versioned = self.checked_version_entry(version_key.clone(), definition)?;
        self.validate_declarative_definition(&versioned)?;
        self.declarative_versions.insert(version_key, versioned);
        Ok(())
    }

    pub fn register_declarative_historical_batch(
        &mut self,
        definitions: impl IntoIterator<Item = DeclarativeWorkflowDefinition>,
    ) -> anyhow::Result<()> {
        let mut staged = self.clone();
        for definition in definitions {
            staged.register_declarative_historical(definition)?;
        }
        *self = staged;
        Ok(())
    }

    fn validate_declarative_definition(
        &self,
        definition: &DeclarativeWorkflowDefinition,
    ) -> anyhow::Result<()> {
        Self::validate_registered_definition(definition.registered())?;
        if definition.registered().id != definition.policy().id {
            anyhow::bail!(
                "declarative workflow registry definition id '{}' does not match policy id '{}'",
                definition.registered().id,
                definition.policy().id
            );
        }
        let (expected_version, expected_hash) =
            declarative_definition_identity(definition.policy())?;
        if definition.definition_version() != expected_version
            || definition.definition_hash() != expected_hash
        {
            anyhow::bail!(
                "declarative workflow definition '{}' identity does not match its canonical policy",
                definition.policy().id
            );
        }
        Ok(())
    }

    fn checked_version_entry(
        &self,
        version_key: (String, u32),
        definition: DeclarativeWorkflowDefinition,
    ) -> anyhow::Result<Arc<DeclarativeWorkflowDefinition>> {
        let Some(existing) = self.declarative_versions.get(&version_key) else {
            return Ok(Arc::new(definition));
        };
        if existing.definition_hash() != definition.definition_hash() {
            anyhow::bail!(
                "declarative workflow definition '{}@{}' version collision between hashes '{}' and '{}'",
                version_key.0,
                version_key.1,
                existing.definition_hash(),
                definition.definition_hash()
            );
        }
        if existing.as_ref() != &definition {
            anyhow::bail!(
                "declarative workflow definition '{}@{}' full hash collision for '{}'",
                version_key.0,
                version_key.1,
                definition.definition_hash()
            );
        }
        Ok(existing.clone())
    }

    pub fn current_declarative_definition(
        &self,
        definition_id: &str,
    ) -> Option<Arc<DeclarativeWorkflowDefinition>> {
        let version = *self.current_declarative_versions.get(definition_id)?;
        self.declarative_definition(definition_id, version)
    }

    pub fn declarative_definition(
        &self,
        definition_id: &str,
        definition_version: u32,
    ) -> Option<Arc<DeclarativeWorkflowDefinition>> {
        self.declarative_versions
            .get(&(definition_id.to_string(), definition_version))
            .cloned()
    }

    pub fn definition_for_version(
        &self,
        definition_id: &str,
        definition_version: u32,
    ) -> Option<Arc<RegisteredWorkflowDefinition>> {
        if let Some(definition) = self.declarative_definition(definition_id, definition_version) {
            return Some(Arc::new(definition.registered().clone()));
        }
        is_builtin_definition_id(definition_id).then(|| self.definition(definition_id))?
    }

    pub fn definition_for_instance(
        &self,
        instance: &WorkflowInstance,
    ) -> Option<Arc<RegisteredWorkflowDefinition>> {
        if self.definitions.contains_key(&instance.definition_id)
            && !self
                .current_declarative_versions
                .contains_key(&instance.definition_id)
        {
            return self.definition(&instance.definition_id);
        }

        match instance.data.get("definition_hash") {
            Some(serde_json::Value::String(expected_hash)) if !expected_hash.trim().is_empty() => {
                let definition = self
                    .declarative_definition(&instance.definition_id, instance.definition_version)?;
                if definition.definition_hash() != expected_hash {
                    return None;
                }
                Some(Arc::new(definition.registered().clone()))
            }
            Some(_) => None,
            None => {
                if let Some(definition) = self
                    .declarative_definition(&instance.definition_id, instance.definition_version)
                {
                    return Some(Arc::new(definition.registered().clone()));
                }
                None
            }
        }
    }

    pub fn state_definition_for_version(
        &self,
        definition_id: &str,
        definition_version: u32,
        state: &str,
    ) -> Option<WorkflowStateDefinition> {
        state_definition(
            self.definition_for_version(definition_id, definition_version)?,
            state,
        )
    }

    pub fn state_definition_for_instance(
        &self,
        instance: &WorkflowInstance,
        state: &str,
    ) -> Option<WorkflowStateDefinition> {
        state_definition(self.definition_for_instance(instance)?, state)
    }
}

fn state_definition(
    definition: Arc<RegisteredWorkflowDefinition>,
    state: &str,
) -> Option<WorkflowStateDefinition> {
    definition
        .states
        .iter()
        .find(|definition| definition.key.state.as_ref() == state)
        .cloned()
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
