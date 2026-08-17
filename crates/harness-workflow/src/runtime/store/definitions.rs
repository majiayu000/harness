use super::{to_jsonb_string, WorkflowRuntimeStore};
use crate::runtime::declarative_pinning::hydrate_persisted_declarative_definition;
use crate::runtime::declarative_pinning::DECLARATIVE_DEFINITION_METADATA_KIND;
use crate::runtime::model::{WorkflowDefinition, WorkflowInstance};
use crate::runtime::{
    DeclarativeWorkflowDefinition, WorkflowDefinitionRegistry, WorkflowTerminalState,
};
use sqlx::{Postgres, Transaction};

impl WorkflowRuntimeStore {
    pub async fn upsert_definition(&self, definition: &WorkflowDefinition) -> anyhow::Result<()> {
        if is_declarative_definition(definition) {
            return self.persist_definition_version(definition).await;
        }
        let data = to_jsonb_string(definition)?;
        let upserted = sqlx::query(
            "INSERT INTO workflow_definitions (id, version, data, active)
             VALUES ($1, $2, $3::jsonb, $4)
             ON CONFLICT (id, version) DO UPDATE SET
                data = EXCLUDED.data,
                active = EXCLUDED.active,
                updated_at = CURRENT_TIMESTAMP
             WHERE workflow_definitions.data->'metadata'->>'kind' IS DISTINCT FROM $5",
        )
        .bind(&definition.id)
        .bind(definition.version as i64)
        .bind(&data)
        .bind(definition.active)
        .bind(DECLARATIVE_DEFINITION_METADATA_KIND)
        .execute(&self.pool)
        .await?;
        if upserted.rows_affected() == 0 {
            anyhow::bail!(
                "workflow definition '{}@{}' is an immutable declarative version and cannot be overwritten through upsert_definition",
                definition.id,
                definition.version
            );
        }
        Ok(())
    }

    pub async fn get_definition(
        &self,
        id: &str,
        version: u32,
    ) -> anyhow::Result<Option<WorkflowDefinition>> {
        let row: Option<(String,)> = sqlx::query_as(
            "SELECT data::text FROM workflow_definitions
             WHERE id = $1 AND version = $2",
        )
        .bind(id)
        .bind(version as i64)
        .fetch_optional(&self.pool)
        .await?;
        row.map(|(data,)| serde_json::from_str(&data))
            .transpose()
            .map_err(Into::into)
    }

    pub async fn terminal_state_for_instance(
        &self,
        instance: &WorkflowInstance,
    ) -> anyhow::Result<Option<WorkflowTerminalState>> {
        if let Some(terminal_state) = self
            .definition_registry
            .terminal_state_for_instance(instance)
        {
            return Ok(Some(terminal_state));
        }
        let definition = self
            .get_definition(&instance.definition_id, instance.definition_version)
            .await?;
        persisted_terminal_state(instance, definition.as_ref())
    }

    pub async fn persist_definition_version(
        &self,
        definition: &WorkflowDefinition,
    ) -> anyhow::Result<()> {
        let data = to_jsonb_string(definition)?;
        let inserted = sqlx::query(
            "INSERT INTO workflow_definitions (id, version, data, active)
             VALUES ($1, $2, $3::jsonb, $4)
             ON CONFLICT (id, version) DO NOTHING",
        )
        .bind(&definition.id)
        .bind(definition.version as i64)
        .bind(&data)
        .bind(definition.active)
        .execute(&self.pool)
        .await?;
        if inserted.rows_affected() == 1 {
            return Ok(());
        }

        let existing = self
            .get_definition(&definition.id, definition.version)
            .await?
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "workflow definition '{}@{}' conflicted but could not be reloaded",
                    definition.id,
                    definition.version
                )
            })?;
        if existing.definition_hash != definition.definition_hash {
            anyhow::bail!(
                "workflow definition '{}@{}' version collision between hashes '{}' and '{}'",
                definition.id,
                definition.version,
                existing.definition_hash,
                definition.definition_hash
            );
        }
        if existing != *definition {
            anyhow::bail!(
                "workflow definition '{}@{}' immutable payload conflicts with the persisted declarative version",
                definition.id,
                definition.version
            );
        }
        Ok(())
    }

    pub async fn list_definitions(&self) -> anyhow::Result<Vec<WorkflowDefinition>> {
        let rows: Vec<(String,)> = sqlx::query_as(
            "SELECT data::text FROM workflow_definitions ORDER BY id ASC, version ASC",
        )
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|(data,)| serde_json::from_str(&data).map_err(Into::into))
            .collect()
    }

    pub async fn list_persisted_declarative_definitions(
        &self,
    ) -> anyhow::Result<Vec<DeclarativeWorkflowDefinition>> {
        self.list_definitions()
            .await?
            .into_iter()
            .filter(is_declarative_definition)
            .map(|definition| hydrate_persisted_declarative_definition(&definition))
            .collect()
    }
}

pub(in crate::runtime) async fn terminal_state_for_instance_tx(
    tx: &mut Transaction<'_, Postgres>,
    registry: &WorkflowDefinitionRegistry,
    instance: &WorkflowInstance,
) -> anyhow::Result<Option<WorkflowTerminalState>> {
    if let Some(terminal_state) = registry.terminal_state_for_instance(instance) {
        return Ok(Some(terminal_state));
    }
    let row: Option<(String,)> = sqlx::query_as(
        "SELECT data::text FROM workflow_definitions
         WHERE id = $1 AND version = $2",
    )
    .bind(&instance.definition_id)
    .bind(i64::from(instance.definition_version))
    .fetch_optional(&mut **tx)
    .await?;
    let definition = row
        .map(|(data,)| serde_json::from_str::<WorkflowDefinition>(&data))
        .transpose()?;
    persisted_terminal_state(instance, definition.as_ref())
}

fn persisted_terminal_state(
    instance: &WorkflowInstance,
    definition: Option<&WorkflowDefinition>,
) -> anyhow::Result<Option<WorkflowTerminalState>> {
    let Some(definition) = definition.filter(|definition| is_declarative_definition(definition))
    else {
        return Ok(None);
    };
    let expected_hash = instance
        .data
        .get("definition_hash")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "declarative workflow '{}' is missing its pinned definition hash",
                instance.id
            )
        })?;
    if definition.id != instance.definition_id
        || definition.version != instance.definition_version
        || definition.definition_hash != expected_hash
    {
        anyhow::bail!(
            "persisted declarative workflow definition '{}@{}' does not match workflow '{}' pin",
            definition.id,
            definition.version,
            instance.id
        );
    }
    let definition = hydrate_persisted_declarative_definition(definition)?;
    Ok(definition
        .registered()
        .states
        .iter()
        .find(|state| state.key.state.as_ref() == instance.state)
        .and_then(|state| state.terminal_state))
}

fn is_declarative_definition(definition: &WorkflowDefinition) -> bool {
    definition
        .metadata
        .get("kind")
        .and_then(serde_json::Value::as_str)
        == Some(DECLARATIVE_DEFINITION_METADATA_KIND)
}
