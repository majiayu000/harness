use super::{
    command_store, insert_decision_record_tx, insert_event_tx, load_or_insert_initial_instance_tx,
    to_jsonb_string, workflow_instance_from_row, WorkflowDecisionTransition, WorkflowInstancePage,
    WorkflowRejectedDecisionTransition, WorkflowRuntimeStore,
};
use crate::runtime::model::{WorkflowDecisionRecord, WorkflowInstance};
use crate::runtime::state_registry::workflow_terminal_state_names_for_definition;
use crate::runtime::status::WorkflowCommandStatus;
use chrono::{DateTime, Utc};

impl WorkflowRuntimeStore {
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

    pub async fn list_nonterminal_instances_by_definition(
        &self,
        definition_id: &str,
        project_id: Option<&str>,
        limit: Option<i64>,
    ) -> anyhow::Result<Vec<WorkflowInstance>> {
        let limit = limit.map(|value| value.clamp(1, 500));
        let terminal_states = workflow_terminal_state_names_for_definition(definition_id);
        let rows: Vec<(String, DateTime<Utc>)> = sqlx::query_as(
            "SELECT data::text, updated_at FROM workflow_instances
             WHERE definition_id = $1
               AND NOT (state = ANY($3::text[]))
               AND ($2::text IS NULL OR data->'data'->>'project_id' = $2)
             ORDER BY updated_at DESC
             LIMIT COALESCE($4, 2147483647)",
        )
        .bind(definition_id)
        .bind(project_id)
        .bind(&terminal_states)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|(data, updated_at)| workflow_instance_from_row(data, updated_at))
            .collect()
    }
}
