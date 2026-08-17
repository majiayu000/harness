use super::{
    instance_helpers::terminal_state_selector_rows, RuntimeHistoryPruneSummary,
    WorkflowRuntimeStore,
};
use chrono::{DateTime, Utc};

/// Shared candidate-selection CTE for retention: terminal workflow root
/// families older than the cutoff that are not depended on by any live
/// workflow. Declarative terminal selectors include the exact version and
/// content hash so a current definition cannot reclassify historical rows.
const PRUNE_ELIGIBLE_ROOTS_CTE: &str = r#"WITH RECURSIVE terminal_states(
                 definition_id, definition_version, definition_hash, state
             ) AS (
                 SELECT * FROM unnest($1::text[], $2::bigint[], $3::text[], $4::text[])
             ),
             candidate_roots AS (
                 SELECT root.id
                 FROM workflow_instances AS root
                 WHERE root.parent_workflow_id IS NULL
                   AND root.updated_at < $5
                   AND EXISTS (
                       SELECT 1
                       FROM terminal_states AS terminal
                       WHERE terminal.definition_id = root.definition_id
                         AND terminal.state = root.state
                         AND (
                             terminal.definition_version IS NULL
                             OR (
                                 terminal.definition_version =
                                     (root.data->>'definition_version')::bigint
                                 AND terminal.definition_hash =
                                     root.data->'data'->>'definition_hash'
                             )
                         )
                   )
                 ORDER BY root.updated_at ASC, root.id ASC
                 LIMIT $6
             ),
             family AS (
                 SELECT root.id AS root_id,
                        root.id,
                        root.definition_id,
                        root.state,
                        root.data,
                        root.updated_at
                 FROM workflow_instances AS root
                 JOIN candidate_roots ON candidate_roots.id = root.id
                 UNION ALL
                 SELECT family.root_id,
                        child.id,
                        child.definition_id,
                        child.state,
                        child.data,
                        child.updated_at
                 FROM workflow_instances AS child
                 JOIN family ON child.parent_workflow_id = family.id
             ),
             eligible_roots AS (
                 SELECT family.root_id
                 FROM family
                 GROUP BY family.root_id
                 HAVING bool_and(family.updated_at < $5)
                    AND bool_and(EXISTS (
                        SELECT 1
                        FROM terminal_states AS terminal
                        WHERE terminal.definition_id = family.definition_id
                          AND terminal.state = family.state
                          AND (
                              terminal.definition_version IS NULL
                              OR (
                                  terminal.definition_version =
                                      (family.data->>'definition_version')::bigint
                                  AND terminal.definition_hash =
                                      family.data->'data'->>'definition_hash'
                              )
                          )
                    ))
                    AND bool_and(NOT EXISTS (
                        SELECT 1
                        FROM workflow_artifact_dependencies AS dependency
                        JOIN workflow_instances AS dependent
                          ON dependent.id = dependency.workflow_id
                        LEFT JOIN workflow_artifacts AS artifact
                          ON artifact.id = dependency.artifact_ref
                        LEFT JOIN runtime_jobs AS producer_job
                          ON producer_job.id = CASE
                              WHEN dependency.artifact_ref LIKE 'runtime-transcript:%'
                              THEN substr(dependency.artifact_ref, length('runtime-transcript:') + 1)
                              ELSE NULL
                          END
                        LEFT JOIN workflow_commands AS producer_command
                          ON producer_command.id = producer_job.command_id
                        WHERE (
                            artifact.workflow_id = family.id
                            OR producer_job.id = family.id
                            OR producer_command.workflow_id = family.id
                            OR dependency.workflow_id = family.id
                        )
                          AND (NOT EXISTS (
                              SELECT 1 FROM terminal_states AS dependent_terminal
                              WHERE dependent_terminal.definition_id = dependent.definition_id
                                AND dependent_terminal.state = dependent.state
                                AND (
                                    dependent_terminal.definition_version IS NULL
                                    OR (
                                        dependent_terminal.definition_version =
                                            (dependent.data->>'definition_version')::bigint
                                        AND dependent_terminal.definition_hash =
                                            dependent.data->'data'->>'definition_hash'
                                    )
                                )
                          ) OR COALESCE(dependent.data->'data'->>'stop_reason_code',
                              dependent.data->'data'->'last_stop'->>'stop_reason_code')
                              = 'runtime_transcript_lost')
                    ))
             )"#;

impl WorkflowRuntimeStore {
    pub async fn count_terminal_history_candidates(
        &self,
        terminal_before: DateTime<Utc>,
        batch_limit: i64,
    ) -> anyhow::Result<u64> {
        let batch_limit = batch_limit.clamp(1, 10_000);
        let selectors = terminal_state_selector_rows(&self.definition_registry);
        if selectors.definition_ids.is_empty() {
            return Ok(0);
        }
        let sql = format!("{PRUNE_ELIGIBLE_ROOTS_CTE} SELECT COUNT(*) FROM eligible_roots");
        let (count,): (i64,) = sqlx::query_as(&sql)
            .bind(&selectors.definition_ids)
            .bind(&selectors.definition_versions)
            .bind(&selectors.definition_hashes)
            .bind(&selectors.states)
            .bind(terminal_before)
            .bind(batch_limit)
            .fetch_one(&self.pool)
            .await?;
        Ok(count.max(0) as u64)
    }

    pub async fn prune_terminal_runtime_history(
        &self,
        terminal_before: DateTime<Utc>,
        batch_limit: i64,
    ) -> anyhow::Result<RuntimeHistoryPruneSummary> {
        let batch_limit = batch_limit.clamp(1, 10_000);
        let selectors = terminal_state_selector_rows(&self.definition_registry);
        if selectors.definition_ids.is_empty() {
            return Ok(RuntimeHistoryPruneSummary::default());
        }
        let rows: Vec<(String,)> = sqlx::query_as(&format!(
            "{PRUNE_ELIGIBLE_ROOTS_CTE} SELECT family.id
             FROM family
             JOIN eligible_roots ON eligible_roots.root_id = family.root_id
             ORDER BY family.root_id ASC, family.id ASC"
        ))
        .bind(&selectors.definition_ids)
        .bind(&selectors.definition_versions)
        .bind(&selectors.definition_hashes)
        .bind(&selectors.states)
        .bind(terminal_before)
        .bind(batch_limit)
        .fetch_all(&self.pool)
        .await?;
        let workflow_ids: Vec<String> = rows.into_iter().map(|(id,)| id).collect();
        if workflow_ids.is_empty() {
            return Ok(RuntimeHistoryPruneSummary::default());
        }
        let mut summary = self
            .runtime_history_counts_for_workflows(&workflow_ids)
            .await?;
        let result = sqlx::query("DELETE FROM workflow_instances WHERE id = ANY($1::text[])")
            .bind(&workflow_ids)
            .execute(&self.pool)
            .await?;
        summary.workflow_instances_deleted = result.rows_affected() as usize;
        Ok(summary)
    }

    async fn runtime_history_counts_for_workflows(
        &self,
        workflow_ids: &[String],
    ) -> anyhow::Result<RuntimeHistoryPruneSummary> {
        let count = |table: &str| {
            format!("SELECT COUNT(*) FROM {table} WHERE workflow_id = ANY($1::text[])")
        };
        let (workflow_instances,): (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM workflow_instances WHERE id = ANY($1::text[])")
                .bind(workflow_ids)
                .fetch_one(&self.pool)
                .await?;
        let (workflow_events,): (i64,) = sqlx::query_as(&count("workflow_events"))
            .bind(workflow_ids)
            .fetch_one(&self.pool)
            .await?;
        let (workflow_decisions,): (i64,) = sqlx::query_as(&count("workflow_decisions"))
            .bind(workflow_ids)
            .fetch_one(&self.pool)
            .await?;
        let (workflow_commands,): (i64,) = sqlx::query_as(&count("workflow_commands"))
            .bind(workflow_ids)
            .fetch_one(&self.pool)
            .await?;
        let (runtime_jobs,): (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM runtime_jobs AS job
             JOIN workflow_commands AS command ON command.id = job.command_id
             WHERE command.workflow_id = ANY($1::text[])",
        )
        .bind(workflow_ids)
        .fetch_one(&self.pool)
        .await?;
        let (runtime_events,): (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM runtime_events AS event
             JOIN runtime_jobs AS job ON job.id = event.runtime_job_id
             JOIN workflow_commands AS command ON command.id = job.command_id
             WHERE command.workflow_id = ANY($1::text[])",
        )
        .bind(workflow_ids)
        .fetch_one(&self.pool)
        .await?;
        let (workflow_artifacts,): (i64,) = sqlx::query_as(&count("workflow_artifacts"))
            .bind(workflow_ids)
            .fetch_one(&self.pool)
            .await?;
        Ok(RuntimeHistoryPruneSummary {
            workflow_instances_deleted: workflow_instances.max(0) as usize,
            workflow_events_deleted: workflow_events.max(0) as usize,
            workflow_decisions_deleted: workflow_decisions.max(0) as usize,
            workflow_commands_deleted: workflow_commands.max(0) as usize,
            runtime_jobs_deleted: runtime_jobs.max(0) as usize,
            runtime_events_deleted: runtime_events.max(0) as usize,
            workflow_artifacts_deleted: workflow_artifacts.max(0) as usize,
        })
    }
}
