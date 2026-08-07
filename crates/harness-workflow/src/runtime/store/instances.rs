use super::{
    commit_parent_attachment_instance_tx, commit_same_state_instance_tx, insert_event_tx,
    insert_validated_canonical_initial_instance_tx,
    instance_helpers::{otel_trace_context_from_data, terminal_state_pairs},
    select_instance_for_update_tx, validate_instance_for_persistence,
    workflow_instance_from_persisted_json, workflow_instance_from_row, RuntimeHistoryPruneSummary,
    WorkflowInstancePage, WorkflowRuntimeStore,
};
use crate::runtime::model::WorkflowInstance;
use crate::runtime::WorkflowOtelTraceContext;
use chrono::{DateTime, Utc};
use serde_json::json;

/// Event recorded when a workflow row is created outside a decision
/// transition, so no creation is eventless (GH-1864).
pub const WORKFLOW_INSTANCE_CREATED_EVENT: &str = "WorkflowInstanceCreated";

/// Shared candidate-selection CTE for retention: terminal workflow root
/// families older than the cutoff that are not depended on by any live
/// workflow. Both the dry-run count and the destructive prune use it so they
/// can never diverge.
const PRUNE_ELIGIBLE_ROOTS_CTE: &str = r#"WITH RECURSIVE terminal_states(definition_id, state) AS (
                 SELECT * FROM unnest($1::text[], $2::text[])
             ),
             candidate_roots AS (
                 SELECT root.id
                 FROM workflow_instances AS root
                 WHERE root.parent_workflow_id IS NULL
                   AND root.updated_at < $3
                   AND EXISTS (
                       SELECT 1
                       FROM terminal_states AS terminal
                       WHERE terminal.definition_id = root.definition_id
                         AND terminal.state = root.state
                   )
                 ORDER BY root.updated_at ASC, root.id ASC
                 LIMIT $4
             ),
             family AS (
                 SELECT root.id AS root_id,
                        root.id,
                        root.definition_id,
                        root.state,
                        root.updated_at
                 FROM workflow_instances AS root
                 JOIN candidate_roots ON candidate_roots.id = root.id
                 UNION ALL
                 SELECT family.root_id,
                        child.id,
                        child.definition_id,
                        child.state,
                        child.updated_at
                 FROM workflow_instances AS child
                 JOIN family ON child.parent_workflow_id = family.id
             ),
             eligible_roots AS (
                 SELECT family.root_id
                 FROM family
                 GROUP BY family.root_id
                 HAVING bool_and(family.updated_at < $3)
                    AND bool_and(EXISTS (
                        SELECT 1
                        FROM terminal_states AS terminal
                        WHERE terminal.definition_id = family.definition_id
                          AND terminal.state = family.state
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
                          ) OR COALESCE(dependent.data->'data'->>'stop_reason_code',
                              dependent.data->'data'->'last_stop'->>'stop_reason_code')
                              = 'runtime_transcript_lost')
                    ))
             )"#;

impl WorkflowRuntimeStore {
    /// Create a workflow at its canonical initial state if it does not exist.
    ///
    /// Creation records a `WorkflowInstanceCreated` event in the same
    /// transaction, so a row can never appear with no provenance for how it
    /// came to exist. Decision-driven creation carries its own event and
    /// decision and does not come through here.
    pub async fn insert_instance_if_absent(
        &self,
        instance: &WorkflowInstance,
    ) -> anyhow::Result<bool> {
        validate_instance_for_persistence(instance)?;
        let mut tx = self.pool.begin().await?;
        let inserted = insert_validated_canonical_initial_instance_tx(&mut tx, instance).await?;
        if inserted {
            record_instance_created_event_tx(&mut tx, instance).await?;
        }
        tx.commit().await?;
        Ok(inserted)
    }

    pub async fn upsert_instance(&self, instance: &WorkflowInstance) -> anyhow::Result<()> {
        validate_instance_for_persistence(instance)?;
        let mut tx = self.pool.begin().await?;
        let current = match select_instance_for_update_tx(&mut tx, &instance.id).await? {
            Some(current) => current,
            None if insert_validated_canonical_initial_instance_tx(&mut tx, instance).await? => {
                record_instance_created_event_tx(&mut tx, instance).await?;
                tx.commit().await?;
                return Ok(());
            }
            None => select_instance_for_update_tx(&mut tx, &instance.id)
                .await?
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "workflow instance `{}` disappeared during guarded public upsert",
                        instance.id
                    )
                })?,
        };
        if current == *instance {
            tx.commit().await?;
            return Ok(());
        }
        ensure_public_upsert_preserves_instance_boundary(&current, instance)?;
        if instance.version == current.version {
            anyhow::bail!(
                "public workflow instance upsert cannot overwrite data at the same version {}; use a state-specific compare-and-swap API",
                current.version
            );
        }
        anyhow::bail!(
            "public workflow instance upsert is insert-only and cannot change version from {} to {}; use a validated decision or state-specific write API",
            current.version,
            instance.version
        )
    }

    pub async fn attach_parent_workflow_if_missing(
        &self,
        workflow_id: &str,
        parent_workflow_id: &str,
    ) -> anyhow::Result<Option<WorkflowInstance>> {
        let mut tx = self.pool.begin().await?;
        let Some(current) = select_instance_for_update_tx(&mut tx, workflow_id).await? else {
            tx.commit().await?;
            return Ok(None);
        };
        let mut instance = current.clone();
        match instance.parent_workflow_id.as_deref() {
            Some(existing) if existing == parent_workflow_id => {
                tx.commit().await?;
                Ok(Some(instance))
            }
            Some(existing) => {
                anyhow::bail!(
                    "workflow instance `{workflow_id}` is already attached to parent `{existing}`"
                );
            }
            None => {
                instance.parent_workflow_id = Some(parent_workflow_id.to_string());
                instance.version = instance.version.saturating_add(1);
                commit_parent_attachment_instance_tx(&mut tx, &current, &instance).await?;
                tx.commit().await?;
                Ok(Some(instance))
            }
        }
    }

    pub async fn ensure_otel_trace_context(
        &self,
        workflow_id: &str,
    ) -> anyhow::Result<Option<WorkflowOtelTraceContext>> {
        let mut tx = self.pool.begin().await?;
        let Some(current) = select_instance_for_update_tx(&mut tx, workflow_id).await? else {
            tx.commit().await?;
            return Ok(None);
        };
        let mut instance = current.clone();
        if let Some(context) = otel_trace_context_from_data(&instance.data) {
            tx.commit().await?;
            return Ok(Some(context));
        }

        let context = WorkflowOtelTraceContext::new();
        instance.set_data_field(
            "otel_trace_context",
            serde_json::to_value(&context)?,
            crate::runtime::DataProvenance::Server,
        )?;
        instance.version = instance.version.saturating_add(1);
        commit_same_state_instance_tx(&mut tx, &current, &instance).await?;
        tx.commit().await?;
        Ok(Some(context))
    }

    /// State-guarded write of the GH-1584 `auto_recovery` attempt-state object
    /// in instance data (`Some` upserts the object, `None` removes it).
    ///
    /// The row is locked (`SELECT ... FOR UPDATE`) and the write only happens
    /// when the instance is still in `expected_state`; otherwise the update is
    /// dropped and `false` is returned so the caller can treat the attempt as
    /// superseded by a concurrent transition (B-009). Returns `false` for
    /// missing instances as well.
    pub async fn set_auto_recovery_state_if_state(
        &self,
        workflow_id: &str,
        expected_state: &str,
        auto_recovery: Option<&serde_json::Value>,
    ) -> anyhow::Result<bool> {
        let mut tx = self.pool.begin().await?;
        let Some(current) = select_instance_for_update_tx(&mut tx, workflow_id).await? else {
            tx.commit().await?;
            return Ok(false);
        };
        let mut instance = current.clone();
        if instance.state != expected_state {
            tx.rollback().await?;
            return Ok(false);
        }
        match auto_recovery {
            Some(value) => {
                instance.set_data_field(
                    "auto_recovery",
                    value.clone(),
                    crate::runtime::DataProvenance::Server,
                )?;
            }
            None => {
                instance
                    .remove_data_field("auto_recovery", crate::runtime::DataProvenance::Server)?;
            }
        }
        instance.version = instance.version.saturating_add(1);
        commit_same_state_instance_tx(&mut tx, &current, &instance).await?;
        tx.commit().await?;
        Ok(true)
    }

    pub async fn get_instance(
        &self,
        workflow_id: &str,
    ) -> anyhow::Result<Option<WorkflowInstance>> {
        let row: Option<(String,)> =
            sqlx::query_as("SELECT data::text FROM workflow_instances WHERE id = $1")
                .bind(workflow_id)
                .fetch_optional(&self.pool)
                .await?;
        row.map(|(data,)| workflow_instance_from_persisted_json(&data))
            .transpose()
    }

    pub async fn get_instance_by_task_id(
        &self,
        task_id: &str,
    ) -> anyhow::Result<Option<WorkflowInstance>> {
        self.get_instance_by_submission_id(task_id).await
    }

    pub async fn get_instance_by_submission_id(
        &self,
        submission_id: &str,
    ) -> anyhow::Result<Option<WorkflowInstance>> {
        let row: Option<(String,)> = sqlx::query_as(
            "SELECT data::text FROM workflow_instances
             WHERE data->'data'->>'submission_id' = $1
                OR (
                    NULLIF(data->'data'->>'submission_id', '') IS NULL
                    AND (
                        data->'data'->>'task_id' = $1
                        OR data->'data'->'task_ids' ? $1
                    )
                )
             ORDER BY
               CASE
                 WHEN data->'data'->>'submission_id' = $1 THEN 0
                 ELSE 1
               END,
               updated_at DESC
             LIMIT 1",
        )
        .bind(submission_id)
        .fetch_optional(&self.pool)
        .await?;
        row.map(|(data,)| workflow_instance_from_persisted_json(&data))
            .transpose()
    }

    pub async fn get_instance_by_pr(
        &self,
        definition_id: &str,
        project_id: &str,
        repo: Option<&str>,
        pr_number: u64,
    ) -> anyhow::Result<Option<WorkflowInstance>> {
        let pr_number = pr_number.to_string();
        let row: Option<(String,)> = sqlx::query_as(
            "SELECT data::text FROM workflow_instances
             WHERE definition_id = $1
               AND data->'data'->>'project_id' = $2
               AND ($3::text IS NULL OR data->'data'->>'repo' = $3)
               AND data->'data'->>'pr_number' = $4
             ORDER BY
               CASE
                 WHEN subject_type = 'issue' OR data->'data' ? 'issue_number' THEN 0
                 ELSE 1
               END,
               updated_at DESC
             LIMIT 1",
        )
        .bind(definition_id)
        .bind(project_id)
        .bind(repo)
        .bind(pr_number)
        .fetch_optional(&self.pool)
        .await?;
        row.map(|(data,)| workflow_instance_from_persisted_json(&data))
            .transpose()
    }

    pub async fn list_instances_by_state(
        &self,
        definition_id: &str,
        state: &str,
        limit: i64,
    ) -> anyhow::Result<Vec<WorkflowInstance>> {
        let limit = limit.clamp(1, 500);
        let rows: Vec<(String,)> = sqlx::query_as(
            "SELECT data::text FROM workflow_instances
             WHERE definition_id = $1
               AND state = $2
             ORDER BY updated_at ASC
             LIMIT $3",
        )
        .bind(definition_id)
        .bind(state)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|(data,)| workflow_instance_from_persisted_json(&data))
            .collect()
    }

    /// Auto-recovery candidate scan (GH-1584): stopped instances of
    /// `definition_id` in `state` whose persisted stop classification is
    /// `transient` and whose repo is in the opted-in allowlist.
    ///
    /// Eligibility is filtered in SQL so ineligible rows (opted-out repos,
    /// terminal or legacy stops, episodes already exhausted) never occupy the
    /// bounded scan window and cannot starve newer eligible instances. Rows
    /// exhausted for a *previous* stop episode stay visible so a fresh
    /// episode can reset its counter. The persisted `reason_class` is only a
    /// coarse pre-filter; the caller re-runs the fail-closed classifier.
    pub async fn list_transient_stopped_candidates(
        &self,
        definition_id: &str,
        state: &str,
        repos: &[String],
        limit: i64,
    ) -> anyhow::Result<Vec<WorkflowInstance>> {
        if repos.is_empty() {
            return Ok(Vec::new());
        }
        let limit = limit.clamp(1, 500);
        let rows: Vec<(String,)> = sqlx::query_as(
            "SELECT data::text FROM workflow_instances
             WHERE definition_id = $1
               AND state = $2
               AND data->'data'->>'repo' = ANY($3::text[])
               AND (data->'data'->>'reason_class' = 'transient'
                    OR data->'data'->'last_stop'->>'reason_class' = 'transient')
               AND NOT (
                    COALESCE(data->'data'->'auto_recovery'->>'exhausted', 'false') = 'true'
                    AND data->'data'->'auto_recovery'->>'episode_event_id'
                        IS NOT DISTINCT FROM data->'data'->'last_stop'->>'event_id'
               )
             ORDER BY updated_at ASC
             LIMIT $4",
        )
        .bind(definition_id)
        .bind(state)
        .bind(repos)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|(data,)| workflow_instance_from_persisted_json(&data))
            .collect()
    }

    pub async fn list_recent_instances_by_state(
        &self,
        definition_id: &str,
        state: &str,
        limit: i64,
    ) -> anyhow::Result<Vec<WorkflowInstance>> {
        let limit = limit.clamp(1, 500);
        let rows: Vec<(String, DateTime<Utc>)> = sqlx::query_as(
            "SELECT data::text, updated_at FROM workflow_instances
             WHERE definition_id = $1
               AND state = $2
             ORDER BY updated_at DESC
             LIMIT $3",
        )
        .bind(definition_id)
        .bind(state)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|(data, updated_at)| workflow_instance_from_row(data, updated_at))
            .collect()
    }

    pub async fn list_aged_wait_instances(
        &self,
        states: &[&str],
        older_than: DateTime<Utc>,
        limit: i64,
    ) -> anyhow::Result<Vec<WorkflowInstance>> {
        if states.is_empty() {
            return Ok(Vec::new());
        }
        let limit = limit.clamp(1, 500);
        let rows: Vec<(String, DateTime<Utc>)> = sqlx::query_as(
            "SELECT data::text, updated_at FROM workflow_instances
             WHERE state = ANY($1::text[])
               AND updated_at < $2
             ORDER BY updated_at ASC, id ASC
             LIMIT $3",
        )
        .bind(states)
        .bind(older_than)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;

        rows.into_iter()
            .map(|(data, updated_at)| workflow_instance_from_row(data, updated_at))
            .filter_map(|result| match result {
                Ok(instance) if !instance.is_terminal() => Some(Ok(instance)),
                Ok(_) => None,
                Err(error) => Some(Err(error)),
            })
            .collect()
    }

    /// Count terminal workflow families eligible for retention pruning,
    /// without deleting anything. Bounded by the same batch limit as
    /// `prune_terminal_runtime_history` so dry-run reports match what the
    /// next real pass would delete.
    pub async fn count_terminal_history_candidates(
        &self,
        terminal_before: DateTime<Utc>,
        batch_limit: i64,
    ) -> anyhow::Result<u64> {
        let batch_limit = batch_limit.clamp(1, 10_000);
        let (terminal_definition_ids, terminal_states) = terminal_state_pairs();
        if terminal_definition_ids.is_empty() {
            return Ok(0);
        }
        let sql = format!("{PRUNE_ELIGIBLE_ROOTS_CTE} SELECT COUNT(*) FROM eligible_roots");
        let (count,): (i64,) = sqlx::query_as(&sql)
            .bind(&terminal_definition_ids)
            .bind(&terminal_states)
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
        let (terminal_definition_ids, terminal_states) = terminal_state_pairs();
        if terminal_definition_ids.is_empty() {
            return Ok(RuntimeHistoryPruneSummary::default());
        }

        let rows: Vec<(String,)> = sqlx::query_as(
            "{PRUNE_ELIGIBLE_ROOTS_CTE} SELECT family.id
             FROM family
             JOIN eligible_roots ON eligible_roots.root_id = family.root_id
             ORDER BY family.root_id ASC, family.id ASC",
        )
        .bind(&terminal_definition_ids)
        .bind(&terminal_states)
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
        let (workflow_instances,): (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM workflow_instances WHERE id = ANY($1::text[])")
                .bind(workflow_ids)
                .fetch_one(&self.pool)
                .await?;
        let (workflow_events,): (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM workflow_events WHERE workflow_id = ANY($1::text[])",
        )
        .bind(workflow_ids)
        .fetch_one(&self.pool)
        .await?;
        let (workflow_decisions,): (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM workflow_decisions WHERE workflow_id = ANY($1::text[])",
        )
        .bind(workflow_ids)
        .fetch_one(&self.pool)
        .await?;
        let (workflow_commands,): (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM workflow_commands WHERE workflow_id = ANY($1::text[])",
        )
        .bind(workflow_ids)
        .fetch_one(&self.pool)
        .await?;
        let (runtime_jobs,): (i64,) = sqlx::query_as(
            "SELECT COUNT(*)
             FROM runtime_jobs AS job
             JOIN workflow_commands AS command ON command.id = job.command_id
             WHERE command.workflow_id = ANY($1::text[])",
        )
        .bind(workflow_ids)
        .fetch_one(&self.pool)
        .await?;
        let (runtime_events,): (i64,) = sqlx::query_as(
            "SELECT COUNT(*)
             FROM runtime_events AS event
             JOIN runtime_jobs AS job ON job.id = event.runtime_job_id
             JOIN workflow_commands AS command ON command.id = job.command_id
             WHERE command.workflow_id = ANY($1::text[])",
        )
        .bind(workflow_ids)
        .fetch_one(&self.pool)
        .await?;
        let (workflow_artifacts,): (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM workflow_artifacts WHERE workflow_id = ANY($1::text[])",
        )
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

    pub async fn touch_instance(&self, workflow_id: &str) -> anyhow::Result<()> {
        sqlx::query("UPDATE workflow_instances SET updated_at = clock_timestamp() WHERE id = $1")
            .bind(workflow_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn list_instances(
        &self,
        project_id: Option<&str>,
        limit: i64,
    ) -> anyhow::Result<Vec<WorkflowInstance>> {
        let page = self.list_instances_page(project_id, limit, 0).await?;
        Ok(page.instances)
    }

    pub async fn list_instances_page(
        &self,
        project_id: Option<&str>,
        limit: i64,
        offset: i64,
    ) -> anyhow::Result<WorkflowInstancePage> {
        let limit = limit.clamp(1, 500);
        let offset = offset.max(0);
        let (total,): (i64,) = sqlx::query_as(
            "SELECT COUNT(*)
             FROM workflow_instances
             WHERE ($1::text IS NULL OR data->'data'->>'project_id' = $1)",
        )
        .bind(project_id)
        .fetch_one(&self.pool)
        .await?;
        let rows: Vec<(String,)> = sqlx::query_as(
            "SELECT data::text FROM workflow_instances
             WHERE ($1::text IS NULL OR data->'data'->>'project_id' = $1)
             ORDER BY updated_at DESC, id DESC
             LIMIT $2 OFFSET $3",
        )
        .bind(project_id)
        .bind(limit)
        .bind(offset)
        .fetch_all(&self.pool)
        .await?;
        let instances = rows
            .into_iter()
            .map(|(data,)| workflow_instance_from_persisted_json(&data))
            .collect::<anyhow::Result<Vec<_>>>()?;
        Ok(WorkflowInstancePage {
            instances,
            total,
            limit,
            offset,
        })
    }

    pub async fn list_instances_by_definition(
        &self,
        definition_id: &str,
        project_id: Option<&str>,
        limit: Option<i64>,
    ) -> anyhow::Result<Vec<WorkflowInstance>> {
        let limit = limit.map(|value| value.clamp(1, 500));
        let rows: Vec<(String,)> = sqlx::query_as(
            "SELECT data::text FROM workflow_instances
             WHERE definition_id = $1
               AND ($2::text IS NULL OR data->'data'->>'project_id' = $2)
             ORDER BY updated_at DESC
             LIMIT COALESCE($3, 2147483647)",
        )
        .bind(definition_id)
        .bind(project_id)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|(data,)| workflow_instance_from_persisted_json(&data))
            .collect()
    }

    /// List instances whose `parent_workflow_id` matches `parent_workflow_id`.
    ///
    /// Scoped to a single parent's children via the dedicated column instead of
    /// scanning a whole definition and filtering in memory.
    pub async fn list_instances_by_parent(
        &self,
        parent_workflow_id: &str,
        limit: Option<i64>,
    ) -> anyhow::Result<Vec<WorkflowInstance>> {
        let limit = limit.map(|value| value.clamp(1, 500));
        let rows: Vec<(String, DateTime<Utc>)> = sqlx::query_as(
            "SELECT data::text, updated_at FROM workflow_instances
             WHERE parent_workflow_id = $1
             ORDER BY updated_at DESC
             LIMIT COALESCE($2, 2147483647)",
        )
        .bind(parent_workflow_id)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|(data, updated_at)| workflow_instance_from_row(data, updated_at))
            .collect()
    }

    pub async fn list_instances_by_definition_page(
        &self,
        definition_id: &str,
        project_id: Option<&str>,
        cursor_created_at: Option<DateTime<Utc>>,
        cursor_id: Option<&str>,
        limit: i64,
    ) -> anyhow::Result<Vec<WorkflowInstance>> {
        let limit = limit.max(1);
        let rows: Vec<(String,)> = sqlx::query_as(
            "SELECT data::text FROM workflow_instances
             WHERE definition_id = $1
               AND ($2::text IS NULL OR data->'data'->>'project_id' = $2)
               AND (
                   $3::timestamptz IS NULL
                   OR (data->>'created_at')::timestamptz < $3
                   OR (
                       (data->>'created_at')::timestamptz = $3
                       AND COALESCE(data->'data'->>'submission_id', data->'data'->'task_ids'->>0, data->'data'->>'task_id', id) < COALESCE($4::text, '')
                   )
               )
             ORDER BY (data->>'created_at')::timestamptz DESC,
                      COALESCE(data->'data'->>'submission_id', data->'data'->'task_ids'->>0, data->'data'->>'task_id', id) DESC
             LIMIT $5",
        )
        .bind(definition_id)
        .bind(project_id)
        .bind(cursor_created_at)
        .bind(cursor_id)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|(data,)| workflow_instance_from_persisted_json(&data))
            .collect()
    }
}

async fn record_instance_created_event_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    instance: &WorkflowInstance,
) -> anyhow::Result<()> {
    insert_event_tx(
        tx,
        &instance.id,
        WORKFLOW_INSTANCE_CREATED_EVENT,
        "workflow_runtime_store",
        json!({
            "definition_id": instance.definition_id,
            "definition_version": instance.definition_version,
            "state": instance.state,
            "subject": instance.subject,
            "parent_workflow_id": instance.parent_workflow_id,
        }),
    )
    .await?;
    Ok(())
}

fn ensure_public_upsert_preserves_instance_boundary(
    current: &WorkflowInstance,
    target: &WorkflowInstance,
) -> anyhow::Result<()> {
    let mut changed_fields = Vec::new();
    if current.definition_id != target.definition_id {
        changed_fields.push("definition_id");
    }
    if current.definition_version != target.definition_version {
        changed_fields.push("definition_version");
    }
    if super::decision_transitions::definition_hash_pin(current)
        != super::decision_transitions::definition_hash_pin(target)
    {
        changed_fields.push("data.definition_hash");
    }
    if current.state != target.state {
        changed_fields.push("state");
    }
    if current.subject != target.subject {
        changed_fields.push("subject");
    }
    if current.parent_workflow_id != target.parent_workflow_id {
        changed_fields.push("parent_workflow_id");
    }
    if current.lease != target.lease {
        changed_fields.push("lease");
    }
    if current.created_at != target.created_at {
        changed_fields.push("created_at");
    }
    if !changed_fields.is_empty() {
        anyhow::bail!(
            "public workflow instance upsert cannot change protected fields: {}; use a decision commit API",
            changed_fields.join(", ")
        );
    }
    Ok(())
}

#[cfg(test)]
#[path = "instances_tests.rs"]
mod tests;
