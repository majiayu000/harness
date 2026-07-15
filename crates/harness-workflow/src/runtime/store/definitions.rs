use super::{to_jsonb_string, WorkflowRuntimeStore};
use crate::runtime::declarative_pinning::DECLARATIVE_DEFINITION_METADATA_KIND;
use crate::runtime::model::WorkflowDefinition;

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
}

fn is_declarative_definition(definition: &WorkflowDefinition) -> bool {
    definition
        .metadata
        .get("kind")
        .and_then(serde_json::Value::as_str)
        == Some(DECLARATIVE_DEFINITION_METADATA_KIND)
}
