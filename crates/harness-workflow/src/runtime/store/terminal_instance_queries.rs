use super::{workflow_instance_from_row, WorkflowInstance, WorkflowRuntimeStore};
use crate::runtime::state_registry::{
    WorkflowProgressStateSelector, WorkflowTerminalStateSelector,
};
use crate::runtime::{WorkflowProgressMode, WorkflowTerminalState};
use chrono::{DateTime, Utc};

struct TerminalSelectorQueryParts {
    unversioned_states: Vec<String>,
    definition_versions: Vec<i64>,
    definition_hashes: Vec<String>,
    versioned_states: Vec<String>,
}

impl WorkflowRuntimeStore {
    pub async fn list_recent_instances_by_progress_mode(
        &self,
        definition_id: &str,
        progress_mode: WorkflowProgressMode,
        limit: i64,
    ) -> anyhow::Result<Vec<WorkflowInstance>> {
        let selectors = self
            .definition_registry
            .progress_state_selectors(definition_id, progress_mode);
        let query = progress_selector_query_parts(&selectors)?;
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
                           AS progress(definition_version, definition_hash, state)
                       WHERE progress.definition_version = (data->>'definition_version')::bigint
                         AND progress.definition_hash = data->'data'->>'definition_hash'
                         AND progress.state = workflow_instances.state
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
        .bind(limit.clamp(1, 500))
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|(data, updated_at)| workflow_instance_from_row(data, updated_at))
            .collect()
    }

    pub async fn list_recent_terminal_instances_by_definition(
        &self,
        definition_id: &str,
        terminal_state: WorkflowTerminalState,
        limit: i64,
    ) -> anyhow::Result<Vec<WorkflowInstance>> {
        self.list_recent_terminal_instances_filtered(definition_id, terminal_state, limit, false)
            .await
    }

    /// Like [`Self::list_recent_terminal_instances_by_definition`], but only
    /// root workflows. The `parent_workflow_id IS NULL` predicate runs before
    /// `LIMIT` so newer child rows cannot crowd out older roots.
    pub async fn list_recent_root_terminal_instances_by_definition(
        &self,
        definition_id: &str,
        terminal_state: WorkflowTerminalState,
        limit: i64,
    ) -> anyhow::Result<Vec<WorkflowInstance>> {
        self.list_recent_terminal_instances_filtered(definition_id, terminal_state, limit, true)
            .await
    }

    async fn list_recent_terminal_instances_filtered(
        &self,
        definition_id: &str,
        terminal_state: WorkflowTerminalState,
        limit: i64,
        roots_only: bool,
    ) -> anyhow::Result<Vec<WorkflowInstance>> {
        let limit = limit.clamp(1, 500);
        let selectors = self
            .definition_registry
            .terminal_state_selectors(definition_id)
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
               AND (NOT $7 OR parent_workflow_id IS NULL)
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
        .bind(roots_only)
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
        self.list_nonterminal_instances_by_definition_filtered(
            definition_id,
            project_id,
            None,
            false,
            limit,
        )
        .await
    }

    /// List nonterminal root workflows older than `updated_before`.
    ///
    /// Age and root-only filters run in SQL so operator diagnostics do not
    /// deserialize every waiting/child instance for a bounded stalled window.
    pub async fn list_aged_root_nonterminal_instances_by_definition(
        &self,
        definition_id: &str,
        updated_before: DateTime<Utc>,
        limit: Option<i64>,
    ) -> anyhow::Result<Vec<WorkflowInstance>> {
        self.list_nonterminal_instances_by_definition_filtered(
            definition_id,
            None,
            Some(updated_before),
            true,
            limit,
        )
        .await
    }

    async fn list_nonterminal_instances_by_definition_filtered(
        &self,
        definition_id: &str,
        project_id: Option<&str>,
        updated_before: Option<DateTime<Utc>>,
        roots_only: bool,
        limit: Option<i64>,
    ) -> anyhow::Result<Vec<WorkflowInstance>> {
        let limit = limit.map(|value| value.clamp(1, 500));
        let selectors = self
            .definition_registry
            .terminal_state_selectors(definition_id);
        let query = terminal_selector_query_parts(&selectors)?;
        let rows: Vec<(String, DateTime<Utc>)> = sqlx::query_as(
            "SELECT data::text, updated_at FROM workflow_instances
             WHERE definition_id = $1
               AND (NOT $9 OR parent_workflow_id IS NULL)
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
               AND ($8::timestamptz IS NULL OR updated_at < $8)
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
        .bind(updated_before)
        .bind(roots_only)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|(data, updated_at)| workflow_instance_from_row(data, updated_at))
            .collect()
    }
}

fn progress_selector_query_parts(
    selectors: &[WorkflowProgressStateSelector],
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
                "progress state selector '{}' must include both definition version and hash",
                selector.state
            ),
        }
    }
    Ok(query)
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

#[cfg(test)]
mod tests {
    use super::super::WorkflowRuntimeStore;
    use crate::runtime::{
        WorkflowInstance, WorkflowSubject, WorkflowTerminalState, GITHUB_ISSUE_PR_DEFINITION_ID,
    };
    use chrono::{Duration, Utc};
    use harness_core::db::resolve_database_url;

    #[tokio::test]
    async fn root_terminal_listing_applies_parent_filter_before_limit() -> anyhow::Result<()> {
        if resolve_database_url(None).is_err() {
            return Ok(());
        }

        let dir = tempfile::tempdir()?;
        let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
        let root = WorkflowInstance::new(
            GITHUB_ISSUE_PR_DEFINITION_ID,
            1,
            "failed",
            WorkflowSubject::new("issue", "issue:root-limit"),
        )
        .with_id("root-failed-before-limit");
        store.force_upsert_lifecycle_state_for_test(&root).await?;
        sqlx::query("UPDATE workflow_instances SET updated_at = $2 WHERE id = $1")
            .bind(&root.id)
            .bind(Utc::now() - Duration::hours(1))
            .execute(store.pool())
            .await?;

        for index in 0..20 {
            let child = WorkflowInstance::new(
                GITHUB_ISSUE_PR_DEFINITION_ID,
                1,
                "failed",
                WorkflowSubject::new("issue", format!("issue:child-limit-{index}")),
            )
            .with_id(format!("child-failed-before-limit-{index}"))
            .with_parent(&root.id);
            store.force_upsert_lifecycle_state_for_test(&child).await?;
        }

        let unfiltered = store
            .list_recent_terminal_instances_by_definition(
                GITHUB_ISSUE_PR_DEFINITION_ID,
                WorkflowTerminalState::Failed,
                20,
            )
            .await?;
        assert!(
            unfiltered.iter().all(|workflow| workflow.id != root.id),
            "unfiltered limit should be filled by newer children: {unfiltered:?}"
        );

        let roots = store
            .list_recent_root_terminal_instances_by_definition(
                GITHUB_ISSUE_PR_DEFINITION_ID,
                WorkflowTerminalState::Failed,
                20,
            )
            .await?;
        assert!(
            roots.iter().any(|workflow| workflow.id == root.id),
            "root filter must run before limit so the older root remains: {roots:?}"
        );
        Ok(())
    }
}
