use super::{workflow_instance_from_row, WorkflowInstance, WorkflowRuntimeStore};
use crate::runtime::state_registry::{
    workflow_terminal_state_selectors_for_definition, WorkflowTerminalStateSelector,
};
use crate::runtime::WorkflowTerminalState;
use chrono::{DateTime, Utc};

struct TerminalSelectorQueryParts {
    unversioned_states: Vec<String>,
    definition_versions: Vec<i64>,
    definition_hashes: Vec<String>,
    versioned_states: Vec<String>,
}

impl WorkflowRuntimeStore {
    pub async fn list_recent_terminal_instances_by_definition(
        &self,
        definition_id: &str,
        terminal_state: WorkflowTerminalState,
        limit: i64,
    ) -> anyhow::Result<Vec<WorkflowInstance>> {
        let limit = limit.clamp(1, 500);
        let selectors = workflow_terminal_state_selectors_for_definition(definition_id)
            .into_iter()
            .filter(|selector| selector.terminal_state == terminal_state)
            .collect::<Vec<_>>();
        let query = terminal_selector_query_parts(&selectors)?;
        if query.unversioned_states.is_empty() && query.versioned_states.is_empty() {
            return Ok(Vec::new());
        }
        let rows: Vec<(String, DateTime<Utc>)> = sqlx::query_as(
            "SELECT data::text, updated_at FROM workflow_instances
             WHERE definition_id = $1
               AND (
                   state = ANY($2::text[])
                   OR EXISTS (
                       SELECT 1
                       FROM unnest($3::bigint[], $4::text[], $5::text[])
                           AS terminal(definition_version, definition_hash, state)
                       WHERE terminal.definition_version = (data->>'definition_version')::bigint
                         AND terminal.definition_hash = data->'data'->>'definition_hash'
                         AND terminal.state = workflow_instances.state
                   )
               )
             ORDER BY updated_at DESC
             LIMIT $6",
        )
        .bind(definition_id)
        .bind(&query.unversioned_states)
        .bind(&query.definition_versions)
        .bind(&query.definition_hashes)
        .bind(&query.versioned_states)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|(data, updated_at)| workflow_instance_from_row(data, updated_at))
            .collect()
    }

    pub async fn list_nonterminal_instances_by_definition(
        &self,
        definition_id: &str,
        project_id: Option<&str>,
        limit: Option<i64>,
    ) -> anyhow::Result<Vec<WorkflowInstance>> {
        let limit = limit.map(|value| value.clamp(1, 500));
        let selectors = workflow_terminal_state_selectors_for_definition(definition_id);
        let query = terminal_selector_query_parts(&selectors)?;
        let rows: Vec<(String, DateTime<Utc>)> = sqlx::query_as(
            "SELECT data::text, updated_at FROM workflow_instances
             WHERE definition_id = $1
               AND NOT (
                   state = ANY($3::text[])
                   OR EXISTS (
                       SELECT 1
                       FROM unnest($4::bigint[], $5::text[], $6::text[])
                           AS terminal(definition_version, definition_hash, state)
                       WHERE terminal.definition_version = (data->>'definition_version')::bigint
                         AND terminal.definition_hash = data->'data'->>'definition_hash'
                         AND terminal.state = workflow_instances.state
                   )
               )
               AND ($2::text IS NULL OR data->'data'->>'project_id' = $2)
             ORDER BY updated_at DESC
             LIMIT COALESCE($7, 2147483647)",
        )
        .bind(definition_id)
        .bind(project_id)
        .bind(&query.unversioned_states)
        .bind(&query.definition_versions)
        .bind(&query.definition_hashes)
        .bind(&query.versioned_states)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|(data, updated_at)| workflow_instance_from_row(data, updated_at))
            .collect()
    }
}

fn terminal_selector_query_parts(
    selectors: &[WorkflowTerminalStateSelector],
) -> anyhow::Result<TerminalSelectorQueryParts> {
    let mut query = TerminalSelectorQueryParts {
        unversioned_states: Vec::new(),
        definition_versions: Vec::new(),
        definition_hashes: Vec::new(),
        versioned_states: Vec::new(),
    };
    for selector in selectors {
        match (selector.definition_version, &selector.definition_hash) {
            (Some(definition_version), Some(definition_hash)) => {
                query
                    .definition_versions
                    .push(i64::from(definition_version));
                query.definition_hashes.push(definition_hash.clone());
                query.versioned_states.push(selector.state.clone());
            }
            (None, None) => query.unversioned_states.push(selector.state.clone()),
            _ => anyhow::bail!(
                "terminal state selector '{}' must include both definition version and hash",
                selector.state
            ),
        }
    }
    Ok(query)
}
