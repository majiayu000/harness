use super::{
    command_store, insert_decision_record_tx, insert_event_tx, insert_instance_if_absent_tx,
    instance_helpers::{otel_trace_context_from_data, terminal_state_pairs},
    load_or_insert_initial_instance_tx, select_instance_for_update_tx, to_jsonb_string,
    upsert_instance_tx, workflow_instance_from_row, RuntimeHistoryPruneSummary,
    WorkflowDecisionTransition, WorkflowInstancePage, WorkflowRejectedDecisionTransition,
    WorkflowRuntimeStore,
};
use crate::runtime::model::{WorkflowDecisionRecord, WorkflowInstance};
use crate::runtime::status::WorkflowCommandStatus;
use crate::runtime::WorkflowOtelTraceContext;
use chrono::{DateTime, Utc};

impl WorkflowRuntimeStore {
    pub async fn insert_instance_if_absent(
        &self,
        instance: &WorkflowInstance,
    ) -> anyhow::Result<bool> {
        let mut tx = self.pool.begin().await?;
        let inserted = insert_instance_if_absent_tx(&mut tx, instance).await?;
        tx.commit().await?;
        Ok(inserted)
    }

    pub async fn upsert_instance(&self, instance: &WorkflowInstance) -> anyhow::Result<()> {
        let data = to_jsonb_string(instance)?;
        sqlx::query(
            "INSERT INTO workflow_instances
                (id, definition_id, state, subject_type, subject_key, parent_workflow_id, data, version)
             VALUES ($1, $2, $3, $4, $5, $6, $7::jsonb, $8)
             ON CONFLICT (id) DO UPDATE SET
                definition_id = EXCLUDED.definition_id,
                state = EXCLUDED.state,
                subject_type = EXCLUDED.subject_type,
                subject_key = EXCLUDED.subject_key,
                parent_workflow_id = EXCLUDED.parent_workflow_id,
                data = EXCLUDED.data,
                version = EXCLUDED.version,
                updated_at = CURRENT_TIMESTAMP",
        )
        .bind(&instance.id)
        .bind(&instance.definition_id)
        .bind(&instance.state)
        .bind(&instance.subject.subject_type)
        .bind(&instance.subject.subject_key)
        .bind(&instance.parent_workflow_id)
        .bind(&data)
        .bind(instance.version as i64)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn ensure_otel_trace_context(
        &self,
        workflow_id: &str,
    ) -> anyhow::Result<Option<WorkflowOtelTraceContext>> {
        let mut tx = self.pool.begin().await?;
        let Some(mut instance) = select_instance_for_update_tx(&mut tx, workflow_id).await? else {
            tx.commit().await?;
            return Ok(None);
        };
        if let Some(context) = otel_trace_context_from_data(&instance.data) {
            tx.commit().await?;
            return Ok(Some(context));
        }

        let context = WorkflowOtelTraceContext::new();
        if !instance.data.is_object() {
            instance.data = serde_json::json!({});
        }
        let data = instance
            .data
            .as_object_mut()
            .ok_or_else(|| anyhow::anyhow!("workflow instance data is not an object"))?;
        data.insert(
            "otel_trace_context".to_string(),
            serde_json::to_value(&context)?,
        );
        instance.version = instance.version.saturating_add(1);
        upsert_instance_tx(&mut tx, &instance).await?;
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
        let Some(mut instance) = select_instance_for_update_tx(&mut tx, workflow_id).await? else {
            tx.commit().await?;
            return Ok(false);
        };
        if instance.state != expected_state {
            tx.rollback().await?;
            return Ok(false);
        }
        if !instance.data.is_object() {
            instance.data = serde_json::json!({});
        }
        let data = instance
            .data
            .as_object_mut()
            .ok_or_else(|| anyhow::anyhow!("workflow instance data is not an object"))?;
        match auto_recovery {
            Some(value) => {
                data.insert("auto_recovery".to_string(), value.clone());
            }
            None => {
                data.remove("auto_recovery");
            }
        }
        instance.version = instance.version.saturating_add(1);
        upsert_instance_tx(&mut tx, &instance).await?;
        tx.commit().await?;
        Ok(true)
    }

    pub async fn apply_decision_transition(
        &self,
        transition: WorkflowDecisionTransition<'_>,
    ) -> anyhow::Result<Option<WorkflowDecisionRecord>> {
        let final_instance = transition.final_instance;
        let decision = transition.decision;
        if decision.workflow_id != final_instance.id {
            anyhow::bail!(
                "workflow decision `{}` targets `{}` but final instance is `{}`",
                decision.decision,
                decision.workflow_id,
                final_instance.id
            );
        }
        let mut tx = self.pool.begin().await?;
        let Some(current) = load_or_insert_initial_instance_tx(
            &mut tx,
            &final_instance.id,
            transition.expected_state,
            transition.create_if_missing,
        )
        .await?
        else {
            return Ok(None);
        };
        if current.is_terminal() || current.state != transition.expected_state {
            return Ok(None);
        }

        let event = insert_event_tx(
            &mut tx,
            &final_instance.id,
            transition.event_type,
            transition.source,
            transition.payload,
        )
        .await?;

        let record = WorkflowDecisionRecord::accepted(decision.clone(), Some(event.id));
        let decision_data = to_jsonb_string(&record)?;
        sqlx::query(
            "INSERT INTO workflow_decisions
                (id, workflow_id, event_id, accepted, data, rejection_reason)
             VALUES ($1, $2, $3, $4, $5::jsonb, $6)
             ON CONFLICT (id) DO UPDATE SET
                accepted = EXCLUDED.accepted,
                data = EXCLUDED.data,
                rejection_reason = EXCLUDED.rejection_reason",
        )
        .bind(&record.id)
        .bind(&record.workflow_id)
        .bind(&record.event_id)
        .bind(record.accepted)
        .bind(&decision_data)
        .bind(&record.rejection_reason)
        .execute(&mut *tx)
        .await?;

        for command in &decision.commands {
            let status = if command.requires_runtime_job() {
                transition.command_status
            } else {
                WorkflowCommandStatus::HandledInline
            };
            command_store::insert_tx(
                &mut tx,
                &final_instance.id,
                Some(&record.id),
                command,
                status,
            )
            .await?;
        }

        let instance_data = to_jsonb_string(final_instance)?;
        sqlx::query(
            "INSERT INTO workflow_instances
                (id, definition_id, state, subject_type, subject_key, parent_workflow_id, data, version)
             VALUES ($1, $2, $3, $4, $5, $6, $7::jsonb, $8)
             ON CONFLICT (id) DO UPDATE SET
                definition_id = EXCLUDED.definition_id,
                state = EXCLUDED.state,
                subject_type = EXCLUDED.subject_type,
                subject_key = EXCLUDED.subject_key,
                parent_workflow_id = EXCLUDED.parent_workflow_id,
                data = EXCLUDED.data,
                version = EXCLUDED.version,
                updated_at = CURRENT_TIMESTAMP",
        )
        .bind(&final_instance.id)
        .bind(&final_instance.definition_id)
        .bind(&final_instance.state)
        .bind(&final_instance.subject.subject_type)
        .bind(&final_instance.subject.subject_key)
        .bind(&final_instance.parent_workflow_id)
        .bind(&instance_data)
        .bind(final_instance.version as i64)
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;
        Ok(Some(record))
    }

    pub async fn record_rejected_decision_transition(
        &self,
        transition: WorkflowRejectedDecisionTransition<'_>,
    ) -> anyhow::Result<Option<WorkflowDecisionRecord>> {
        let decision = transition.decision;
        let mut tx = self.pool.begin().await?;
        let Some(current) = load_or_insert_initial_instance_tx(
            &mut tx,
            &decision.workflow_id,
            transition.expected_state,
            transition.create_if_missing,
        )
        .await?
        else {
            return Ok(None);
        };
        if current.is_terminal() || current.state != transition.expected_state {
            return Ok(None);
        }

        let event = insert_event_tx(
            &mut tx,
            &decision.workflow_id,
            transition.event_type,
            transition.source,
            transition.payload,
        )
        .await?;
        let record =
            WorkflowDecisionRecord::rejected(decision.clone(), Some(event.id), transition.reason);
        insert_decision_record_tx(&mut tx, &record).await?;

        tx.commit().await?;
        Ok(Some(record))
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
        row.map(|(data,)| serde_json::from_str(&data))
            .transpose()
            .map_err(Into::into)
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
        row.map(|(data,)| serde_json::from_str(&data))
            .transpose()
            .map_err(Into::into)
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
        row.map(|(data,)| serde_json::from_str(&data))
            .transpose()
            .map_err(Into::into)
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
            .map(|(data,)| Ok(serde_json::from_str(&data)?))
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
            .map(|(data,)| Ok(serde_json::from_str(&data)?))
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
            "WITH RECURSIVE terminal_states(definition_id, state) AS (
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
                        FROM workflow_artifacts AS artifact
                        JOIN workflow_artifact_dependencies AS dependency
                          ON dependency.artifact_ref = artifact.id
                        JOIN workflow_instances AS dependent
                          ON dependent.id = dependency.workflow_id
                        WHERE artifact.workflow_id = family.id
                          AND NOT EXISTS (
                              SELECT 1
                              FROM terminal_states AS dependent_terminal
                              WHERE dependent_terminal.definition_id = dependent.definition_id
                                AND dependent_terminal.state = dependent.state
                          )
                    ))
             )
             SELECT family.id
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
            .map(|(data,)| Ok(serde_json::from_str(&data)?))
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
            .map(|(data,)| Ok(serde_json::from_str(&data)?))
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
        let rows: Vec<(String,)> = sqlx::query_as(
            "SELECT data::text FROM workflow_instances
             WHERE parent_workflow_id = $1
             ORDER BY updated_at DESC
             LIMIT COALESCE($2, 2147483647)",
        )
        .bind(parent_workflow_id)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|(data,)| Ok(serde_json::from_str(&data)?))
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
            .map(|(data,)| Ok(serde_json::from_str(&data)?))
            .collect()
    }
}
