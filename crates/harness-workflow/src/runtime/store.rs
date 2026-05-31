use super::model::{
    ActivityResult, ActivityStatus, RuntimeEvent, RuntimeJob, RuntimeJobStatus, RuntimeKind,
    WorkflowCommand, WorkflowCommandRecord, WorkflowCommandType, WorkflowDecision,
    WorkflowDecisionRecord, WorkflowDefinition, WorkflowEvent, WorkflowInstance,
};
use super::pr_feedback::PR_FEEDBACK_DEFINITION_ID;
use super::prompt_task::PROMPT_TASK_DEFINITION_ID;
use super::quality_gate::QUALITY_GATE_DEFINITION_ID;
use super::reducer::{reduce_runtime_job_completed, GITHUB_ISSUE_PR_DEFINITION_ID};
use super::repo_backlog::REPO_BACKLOG_DEFINITION_ID;
use super::store_migrations::WORKFLOW_RUNTIME_MIGRATIONS;
use super::validator::{DecisionValidator, ValidationContext};
use anyhow::Context;
use chrono::{DateTime, Utc};
use harness_core::db::PgStoreContext;
use serde::Serialize;
use serde_json::{json, Value};
use sqlx::postgres::PgPool;
use std::collections::BTreeMap;
use std::path::Path;
use uuid::Uuid;

const COMMAND_STATUS_HANDLED_INLINE: &str = "handled_inline";

pub struct WorkflowRuntimeStore {
    pub(super) pool: PgPool,
}

pub struct WorkflowInstancePage {
    pub instances: Vec<WorkflowInstance>,
    pub total: i64,
    pub limit: i64,
    pub offset: i64,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct WorkflowRuntimeSummaryCounts {
    pub total_commands: usize,
    pub total_runtime_jobs: usize,
    pub command_statuses: BTreeMap<String, usize>,
    pub runtime_job_statuses: BTreeMap<String, usize>,
    pub activity_outcomes: BTreeMap<String, usize>,
    pub jobs_without_activity_envelope: usize,
}

pub struct WorkflowDecisionTransition<'a> {
    pub expected_state: &'a str,
    pub create_if_missing: Option<&'a WorkflowInstance>,
    pub event_type: &'a str,
    pub source: &'a str,
    pub payload: Value,
    pub decision: &'a WorkflowDecision,
    pub final_instance: &'a WorkflowInstance,
    pub command_status: &'a str,
}

pub struct WorkflowRejectedDecisionTransition<'a> {
    pub expected_state: &'a str,
    pub create_if_missing: Option<&'a WorkflowInstance>,
    pub event_type: &'a str,
    pub source: &'a str,
    pub payload: Value,
    pub decision: &'a WorkflowDecision,
    pub reason: &'a str,
}

type WorkflowCommandRecordRow = (
    String,
    String,
    Option<String>,
    String,
    Option<String>,
    Option<DateTime<Utc>>,
    String,
    DateTime<Utc>,
    DateTime<Utc>,
);

#[derive(Debug, Clone, PartialEq)]
pub enum RuntimeJobEnqueueOutcome {
    Enqueued(RuntimeJob),
    AlreadyExists(RuntimeJob),
    CommandNotPending { status: String },
}

#[derive(Debug, Clone, PartialEq)]
pub struct RuntimeActivityCompletion {
    pub runtime_job: RuntimeJob,
    pub command: Option<WorkflowCommandRecord>,
    pub workflow_event: Option<WorkflowEvent>,
    pub decision: Option<WorkflowDecisionRecord>,
}

fn workflow_command_record_from_row(
    (
        id,
        workflow_id,
        decision_id,
        status,
        dispatch_owner,
        dispatch_lease_expires_at,
        data,
        created_at,
        updated_at,
    ): WorkflowCommandRecordRow,
) -> anyhow::Result<WorkflowCommandRecord> {
    Ok(WorkflowCommandRecord {
        id,
        workflow_id,
        decision_id,
        status,
        dispatch_owner,
        dispatch_lease_expires_at,
        command: serde_json::from_str(&data)?,
        created_at,
        updated_at,
    })
}

impl WorkflowRuntimeStore {
    pub async fn open(path: &Path) -> anyhow::Result<Self> {
        Self::open_with_database_url(path, None).await
    }

    pub async fn open_with_database_url(
        path: &Path,
        configured_database_url: Option<&str>,
    ) -> anyhow::Result<Self> {
        let context = PgStoreContext::from_path(path, configured_database_url)?;
        let pool = context
            .open_migrated_pool(WORKFLOW_RUNTIME_MIGRATIONS)
            .await?;
        Ok(Self { pool })
    }

    pub async fn open_with_database_url_and_schema(
        configured_database_url: Option<&str>,
        schema: &str,
    ) -> anyhow::Result<Self> {
        let context = PgStoreContext::from_schema(schema, configured_database_url)?;
        let pool = context
            .open_migrated_pool(WORKFLOW_RUNTIME_MIGRATIONS)
            .await?;
        Ok(Self { pool })
    }

    pub async fn open_with_context(
        context: &PgStoreContext,
        setup_pool: &PgPool,
    ) -> anyhow::Result<Self> {
        let pool = context
            .open_migrated_pool_with_setup_pool(setup_pool, WORKFLOW_RUNTIME_MIGRATIONS)
            .await?;
        Ok(Self { pool })
    }

    pub async fn upsert_definition(&self, definition: &WorkflowDefinition) -> anyhow::Result<()> {
        let data = to_jsonb_string(definition)?;
        sqlx::query(
            "INSERT INTO workflow_definitions (id, version, data, active)
             VALUES ($1, $2, $3::jsonb, $4)
             ON CONFLICT (id, version) DO UPDATE SET
                data = EXCLUDED.data,
                active = EXCLUDED.active,
                updated_at = CURRENT_TIMESTAMP",
        )
        .bind(&definition.id)
        .bind(definition.version as i64)
        .bind(&data)
        .bind(definition.active)
        .execute(&self.pool)
        .await?;
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

    pub async fn upsert_prompt_payload(
        &self,
        prompt_ref: &str,
        prompt: &str,
    ) -> anyhow::Result<()> {
        if prompt_ref.trim().is_empty() {
            anyhow::bail!("workflow prompt payload prompt_ref must not be empty");
        }
        sqlx::query(
            "INSERT INTO workflow_prompt_payloads (prompt_ref, prompt)
             VALUES ($1, $2)
             ON CONFLICT (prompt_ref) DO UPDATE SET
                prompt = EXCLUDED.prompt,
                updated_at = CURRENT_TIMESTAMP",
        )
        .bind(prompt_ref)
        .bind(prompt)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn get_prompt_payload(&self, prompt_ref: &str) -> anyhow::Result<Option<String>> {
        if prompt_ref.trim().is_empty() {
            return Ok(None);
        }
        let row: Option<(String,)> =
            sqlx::query_as("SELECT prompt FROM workflow_prompt_payloads WHERE prompt_ref = $1")
                .bind(prompt_ref)
                .fetch_optional(&self.pool)
                .await?;
        Ok(row.map(|(prompt,)| prompt))
    }

    pub async fn delete_prompt_payload(&self, prompt_ref: &str) -> anyhow::Result<()> {
        if prompt_ref.trim().is_empty() {
            return Ok(());
        }
        sqlx::query("DELETE FROM workflow_prompt_payloads WHERE prompt_ref = $1")
            .bind(prompt_ref)
            .execute(&self.pool)
            .await?;
        Ok(())
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
            let command_data = to_jsonb_string(command)?;
            let command_type = enum_str(&command.command_type)?;
            sqlx::query(
                "INSERT INTO workflow_commands
                    (id, workflow_id, decision_id, command_type, dedupe_key, status, data)
                 VALUES ($1, $2, $3, $4, $5, $6, $7::jsonb)
                 ON CONFLICT (workflow_id, dedupe_key) DO UPDATE SET
                    status = CASE
                        WHEN workflow_commands.status = 'pending' THEN EXCLUDED.status
                        ELSE workflow_commands.status
                    END,
                    updated_at = CASE
                        WHEN workflow_commands.status = 'pending'
                             AND workflow_commands.status <> EXCLUDED.status
                        THEN CURRENT_TIMESTAMP
                        ELSE workflow_commands.updated_at
                    END",
            )
            .bind(Uuid::new_v4().to_string())
            .bind(&final_instance.id)
            .bind(&record.id)
            .bind(&command_type)
            .bind(&command.dedupe_key)
            .bind(transition.command_status)
            .bind(&command_data)
            .execute(&mut *tx)
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
        let row: Option<(String,)> = sqlx::query_as(
            "SELECT data::text FROM workflow_instances
             WHERE data->'data'->>'task_id' = $1
                OR data->'data'->'task_ids' ? $1
             ORDER BY updated_at DESC
             LIMIT 1",
        )
        .bind(task_id)
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
                       AND COALESCE(data->'data'->'task_ids'->>0, data->'data'->>'task_id', id) < COALESCE($4::text, '')
                   )
               )
             ORDER BY (data->>'created_at')::timestamptz DESC,
                      COALESCE(data->'data'->'task_ids'->>0, data->'data'->>'task_id', id) DESC
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
        let rows: Vec<(String,)> = sqlx::query_as(
            "SELECT data::text FROM workflow_instances
             WHERE definition_id = $1
               AND state NOT IN ('done', 'passed', 'failed', 'cancelled')
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

    pub async fn append_event(
        &self,
        workflow_id: &str,
        event_type: &str,
        source: &str,
        payload: Value,
    ) -> anyhow::Result<WorkflowEvent> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
            .bind(format!("workflow_events:{workflow_id}"))
            .execute(&mut *tx)
            .await?;
        let (next_sequence,): (i64,) = sqlx::query_as(
            "SELECT COALESCE(MAX(sequence), 0) + 1 FROM workflow_events WHERE workflow_id = $1",
        )
        .bind(workflow_id)
        .fetch_one(&mut *tx)
        .await?;
        let event = WorkflowEvent::new(workflow_id, next_sequence as u64, event_type, source)
            .with_payload(payload);
        let data = to_jsonb_string(&event)?;
        sqlx::query(
            "INSERT INTO workflow_events
                (id, workflow_id, sequence, event_type, source, data)
             VALUES ($1, $2, $3, $4, $5, $6::jsonb)",
        )
        .bind(&event.id)
        .bind(&event.workflow_id)
        .bind(event.sequence as i64)
        .bind(&event.event_type)
        .bind(&event.source)
        .bind(&data)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(event)
    }

    pub async fn events_for(&self, workflow_id: &str) -> anyhow::Result<Vec<WorkflowEvent>> {
        let rows: Vec<(String,)> = sqlx::query_as(
            "SELECT data::text FROM workflow_events
             WHERE workflow_id = $1
             ORDER BY sequence ASC",
        )
        .bind(workflow_id)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|(data,)| Ok(serde_json::from_str(&data)?))
            .collect()
    }

    pub async fn latest_event_for_type(
        &self,
        workflow_id: &str,
        event_type: &str,
    ) -> anyhow::Result<Option<WorkflowEvent>> {
        let row: Option<(String,)> = sqlx::query_as(
            "SELECT data::text FROM workflow_events
             WHERE workflow_id = $1
               AND event_type = $2
             ORDER BY sequence DESC
             LIMIT 1",
        )
        .bind(workflow_id)
        .bind(event_type)
        .fetch_optional(&self.pool)
        .await?;
        row.map(|(data,)| Ok(serde_json::from_str(&data)?))
            .transpose()
    }

    pub async fn events_for_workflows(
        &self,
        workflow_ids: &[String],
    ) -> anyhow::Result<BTreeMap<String, Vec<WorkflowEvent>>> {
        if workflow_ids.is_empty() {
            return Ok(BTreeMap::new());
        }
        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT workflow_id, data::text FROM workflow_events
             WHERE workflow_id = ANY($1::text[])
             ORDER BY workflow_id ASC, sequence ASC",
        )
        .bind(workflow_ids)
        .fetch_all(&self.pool)
        .await?;
        let mut by_workflow = BTreeMap::new();
        for (workflow_id, data) in rows {
            by_workflow
                .entry(workflow_id)
                .or_insert_with(Vec::new)
                .push(serde_json::from_str(&data)?);
        }
        Ok(by_workflow)
    }

    pub async fn record_decision(&self, record: &WorkflowDecisionRecord) -> anyhow::Result<()> {
        let data = to_jsonb_string(record)?;
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
        .bind(&data)
        .bind(&record.rejection_reason)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn decisions_for(
        &self,
        workflow_id: &str,
    ) -> anyhow::Result<Vec<WorkflowDecisionRecord>> {
        let rows: Vec<(String,)> = sqlx::query_as(
            "SELECT data::text FROM workflow_decisions
             WHERE workflow_id = $1
             ORDER BY created_at ASC",
        )
        .bind(workflow_id)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|(data,)| Ok(serde_json::from_str(&data)?))
            .collect()
    }

    pub async fn decisions_for_workflows(
        &self,
        workflow_ids: &[String],
    ) -> anyhow::Result<BTreeMap<String, Vec<WorkflowDecisionRecord>>> {
        if workflow_ids.is_empty() {
            return Ok(BTreeMap::new());
        }
        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT workflow_id, data::text FROM workflow_decisions
             WHERE workflow_id = ANY($1::text[])
             ORDER BY workflow_id ASC, created_at ASC",
        )
        .bind(workflow_ids)
        .fetch_all(&self.pool)
        .await?;
        let mut by_workflow = BTreeMap::new();
        for (workflow_id, data) in rows {
            by_workflow
                .entry(workflow_id)
                .or_insert_with(Vec::new)
                .push(serde_json::from_str(&data)?);
        }
        Ok(by_workflow)
    }

    pub async fn enqueue_command(
        &self,
        workflow_id: &str,
        decision_id: Option<&str>,
        command: &WorkflowCommand,
    ) -> anyhow::Result<String> {
        self.enqueue_command_with_status(workflow_id, decision_id, command, "pending")
            .await
    }

    pub async fn enqueue_command_with_status(
        &self,
        workflow_id: &str,
        decision_id: Option<&str>,
        command: &WorkflowCommand,
        status: &str,
    ) -> anyhow::Result<String> {
        let data = to_jsonb_string(command)?;
        let command_type = enum_str(&command.command_type)?;
        let (id,): (String,) = sqlx::query_as(
            "INSERT INTO workflow_commands
                (id, workflow_id, decision_id, command_type, dedupe_key, status, data)
             VALUES ($1, $2, $3, $4, $5, $6, $7::jsonb)
             ON CONFLICT (workflow_id, dedupe_key) DO UPDATE SET
                status = CASE
                    WHEN workflow_commands.status = 'pending' THEN EXCLUDED.status
                    ELSE workflow_commands.status
                END,
                updated_at = CASE
                    WHEN workflow_commands.status = 'pending'
                         AND workflow_commands.status <> EXCLUDED.status
                    THEN CURRENT_TIMESTAMP
                    ELSE workflow_commands.updated_at
                END
             RETURNING id",
        )
        .bind(Uuid::new_v4().to_string())
        .bind(workflow_id)
        .bind(decision_id)
        .bind(&command_type)
        .bind(&command.dedupe_key)
        .bind(status)
        .bind(&data)
        .fetch_one(&self.pool)
        .await?;
        Ok(id)
    }

    pub async fn commands_for(
        &self,
        workflow_id: &str,
    ) -> anyhow::Result<Vec<WorkflowCommandRecord>> {
        let rows: Vec<WorkflowCommandRecordRow> = sqlx::query_as(
            "SELECT id, workflow_id, decision_id, status, dispatch_owner,
                    dispatch_lease_expires_at, data::text, created_at, updated_at
                 FROM workflow_commands
                 WHERE workflow_id = $1
                 ORDER BY created_at ASC",
        )
        .bind(workflow_id)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(workflow_command_record_from_row)
            .collect()
    }

    pub async fn commands_for_workflows(
        &self,
        workflow_ids: &[String],
    ) -> anyhow::Result<BTreeMap<String, Vec<WorkflowCommandRecord>>> {
        if workflow_ids.is_empty() {
            return Ok(BTreeMap::new());
        }
        let rows: Vec<WorkflowCommandRecordRow> = sqlx::query_as(
            "SELECT id, workflow_id, decision_id, status, dispatch_owner,
                    dispatch_lease_expires_at, data::text, created_at, updated_at
             FROM workflow_commands
             WHERE workflow_id = ANY($1::text[])
             ORDER BY workflow_id ASC, created_at ASC",
        )
        .bind(workflow_ids)
        .fetch_all(&self.pool)
        .await?;
        let mut by_workflow = BTreeMap::new();
        for row in rows {
            let record = workflow_command_record_from_row(row)?;
            by_workflow
                .entry(record.workflow_id.clone())
                .or_insert_with(Vec::new)
                .push(record);
        }
        Ok(by_workflow)
    }

    pub async fn get_command(
        &self,
        command_id: &str,
    ) -> anyhow::Result<Option<WorkflowCommandRecord>> {
        let row: Option<WorkflowCommandRecordRow> = sqlx::query_as(
            "SELECT id, workflow_id, decision_id, status, dispatch_owner,
                    dispatch_lease_expires_at, data::text, created_at, updated_at
             FROM workflow_commands
             WHERE id = $1",
        )
        .bind(command_id)
        .fetch_optional(&self.pool)
        .await?;
        row.map(workflow_command_record_from_row).transpose()
    }

    pub async fn pending_commands(&self, limit: i64) -> anyhow::Result<Vec<WorkflowCommandRecord>> {
        let limit = limit.clamp(1, 500);
        let rows: Vec<WorkflowCommandRecordRow> = sqlx::query_as(
            "SELECT id, workflow_id, decision_id, status, dispatch_owner,
                    dispatch_lease_expires_at, data::text, created_at, updated_at
             FROM workflow_commands
             WHERE status = 'pending'
             ORDER BY created_at ASC
             LIMIT $1",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(workflow_command_record_from_row)
            .collect()
    }

    pub async fn claim_pending_commands(
        &self,
        owner: &str,
        expires_at: DateTime<Utc>,
        limit: i64,
    ) -> anyhow::Result<Vec<WorkflowCommandRecord>> {
        let limit = limit.clamp(1, 500);
        let rows: Vec<WorkflowCommandRecordRow> = sqlx::query_as(
            "WITH candidates AS (
                 SELECT id
                 FROM workflow_commands
                 WHERE status = 'pending'
                    OR (
                        status = 'dispatching'
                        AND COALESCE(dispatch_lease_expires_at, '-infinity'::timestamptz)
                            <= CURRENT_TIMESTAMP
                    )
                 ORDER BY created_at ASC
                 LIMIT $3
                 FOR UPDATE SKIP LOCKED
             )
             UPDATE workflow_commands AS command
             SET status = 'dispatching',
                 dispatch_owner = $1,
                 dispatch_lease_expires_at = $2,
                 updated_at = CURRENT_TIMESTAMP
             FROM candidates
             WHERE command.id = candidates.id
             RETURNING command.id, command.workflow_id, command.decision_id, command.status,
                       command.dispatch_owner, command.dispatch_lease_expires_at,
                       command.data::text, command.created_at, command.updated_at",
        )
        .bind(owner)
        .bind(expires_at)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        let mut records: Vec<_> = rows
            .into_iter()
            .map(workflow_command_record_from_row)
            .collect::<anyhow::Result<_>>()?;
        records.sort_by_key(|record| record.created_at);
        Ok(records)
    }

    pub async fn mark_command_status(&self, command_id: &str, status: &str) -> anyhow::Result<()> {
        sqlx::query(
            "UPDATE workflow_commands
             SET status = $1,
                 dispatch_owner = NULL,
                 dispatch_lease_expires_at = NULL,
                 updated_at = CURRENT_TIMESTAMP
             WHERE id = $2",
        )
        .bind(status)
        .bind(command_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn mark_pending_command_status(
        &self,
        command_id: &str,
        status: &str,
    ) -> anyhow::Result<bool> {
        let result = sqlx::query(
            "UPDATE workflow_commands
             SET status = $1,
                 dispatch_owner = NULL,
                 dispatch_lease_expires_at = NULL,
                 updated_at = CURRENT_TIMESTAMP
             WHERE id = $2 AND status = 'pending'",
        )
        .bind(status)
        .bind(command_id)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    pub async fn enqueue_runtime_job(
        &self,
        command_id: &str,
        runtime_kind: RuntimeKind,
        runtime_profile: &str,
        input: Value,
    ) -> anyhow::Result<RuntimeJob> {
        self.enqueue_runtime_job_with_not_before(
            command_id,
            runtime_kind,
            runtime_profile,
            input,
            None,
        )
        .await
    }

    pub async fn enqueue_runtime_job_with_not_before(
        &self,
        command_id: &str,
        runtime_kind: RuntimeKind,
        runtime_profile: &str,
        input: Value,
        not_before: Option<DateTime<Utc>>,
    ) -> anyhow::Result<RuntimeJob> {
        let mut job = RuntimeJob::pending(command_id, runtime_kind, runtime_profile, input);
        job.not_before = not_before;
        let data = to_jsonb_string(&job)?;
        let status = enum_str(&job.status)?;
        let runtime_kind = enum_str(&job.runtime_kind)?;
        sqlx::query(
            "INSERT INTO runtime_jobs
                (id, command_id, runtime_kind, runtime_profile, status, not_before, data)
             VALUES ($1, $2, $3, $4, $5, $6, $7::jsonb)",
        )
        .bind(&job.id)
        .bind(&job.command_id)
        .bind(&runtime_kind)
        .bind(&job.runtime_profile)
        .bind(&status)
        .bind(job.not_before)
        .bind(&data)
        .execute(&self.pool)
        .await?;
        Ok(job)
    }

    pub async fn enqueue_runtime_job_for_pending_command(
        &self,
        command_id: &str,
        runtime_kind: RuntimeKind,
        runtime_profile: &str,
        input: Value,
        not_before: Option<DateTime<Utc>>,
    ) -> anyhow::Result<RuntimeJobEnqueueOutcome> {
        self.enqueue_runtime_job_for_command(
            command_id,
            None,
            runtime_kind,
            runtime_profile,
            input,
            not_before,
        )
        .await
    }

    pub async fn enqueue_runtime_job_for_claimed_command(
        &self,
        command_id: &str,
        dispatch_owner: &str,
        runtime_kind: RuntimeKind,
        runtime_profile: &str,
        input: Value,
        not_before: Option<DateTime<Utc>>,
    ) -> anyhow::Result<RuntimeJobEnqueueOutcome> {
        self.enqueue_runtime_job_for_command(
            command_id,
            Some(dispatch_owner),
            runtime_kind,
            runtime_profile,
            input,
            not_before,
        )
        .await
    }

    async fn enqueue_runtime_job_for_command(
        &self,
        command_id: &str,
        dispatch_owner: Option<&str>,
        runtime_kind: RuntimeKind,
        runtime_profile: &str,
        input: Value,
        not_before: Option<DateTime<Utc>>,
    ) -> anyhow::Result<RuntimeJobEnqueueOutcome> {
        let mut job = RuntimeJob::pending(command_id, runtime_kind, runtime_profile, input);
        job.not_before = not_before;
        let data = to_jsonb_string(&job)?;
        let status = enum_str(&job.status)?;
        let runtime_kind = enum_str(&job.runtime_kind)?;
        let mut tx = self.pool.begin().await?;
        let command_row: Option<(String, Option<String>)> = sqlx::query_as(
            "SELECT status, dispatch_owner FROM workflow_commands WHERE id = $1 FOR UPDATE",
        )
        .bind(command_id)
        .fetch_optional(&mut *tx)
        .await?;
        let Some((command_status, command_dispatch_owner)) = command_row else {
            anyhow::bail!("workflow command not found: {command_id}");
        };

        let eligible = match dispatch_owner {
            Some(owner) => {
                command_status == "dispatching" && command_dispatch_owner.as_deref() == Some(owner)
            }
            None => command_status == "pending",
        };

        if !eligible {
            let existing = runtime_job_for_command_tx(&mut tx, command_id).await?;
            tx.commit().await?;
            return Ok(match existing {
                Some(runtime_job) => RuntimeJobEnqueueOutcome::AlreadyExists(runtime_job),
                None => RuntimeJobEnqueueOutcome::CommandNotPending {
                    status: command_status,
                },
            });
        }

        if let Some(existing) = runtime_job_for_command_tx(&mut tx, command_id).await? {
            sqlx::query(
                "UPDATE workflow_commands
                 SET status = 'dispatched',
                     dispatch_owner = NULL,
                     dispatch_lease_expires_at = NULL,
                     updated_at = CURRENT_TIMESTAMP
                 WHERE id = $1",
            )
            .bind(command_id)
            .execute(&mut *tx)
            .await?;
            tx.commit().await?;
            return Ok(RuntimeJobEnqueueOutcome::AlreadyExists(existing));
        }

        sqlx::query(
            "INSERT INTO runtime_jobs
                (id, command_id, runtime_kind, runtime_profile, status, not_before, data)
             VALUES ($1, $2, $3, $4, $5, $6, $7::jsonb)",
        )
        .bind(&job.id)
        .bind(&job.command_id)
        .bind(&runtime_kind)
        .bind(&job.runtime_profile)
        .bind(&status)
        .bind(job.not_before)
        .bind(&data)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "UPDATE workflow_commands
             SET status = 'dispatched',
                 dispatch_owner = NULL,
                 dispatch_lease_expires_at = NULL,
                 updated_at = CURRENT_TIMESTAMP
             WHERE id = $1",
        )
        .bind(command_id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(RuntimeJobEnqueueOutcome::Enqueued(job))
    }

    pub async fn claim_next_runtime_job(
        &self,
        owner: &str,
        expires_at: chrono::DateTime<chrono::Utc>,
    ) -> anyhow::Result<Option<RuntimeJob>> {
        let mut tx = self.pool.begin().await?;
        let row: Option<(String, String)> = sqlx::query_as(
            "SELECT id, data::text FROM runtime_jobs
             WHERE (
                 status = 'pending'
                 AND (not_before IS NULL OR not_before <= CURRENT_TIMESTAMP)
             ) OR (
                 status = 'running'
                 AND data ? 'lease'
                 AND (data->'lease' ? 'expires_at')
                 AND (data->'lease'->>'expires_at')::timestamptz <= CURRENT_TIMESTAMP
             )
             ORDER BY
                 CASE
                     WHEN COALESCE(data #>> '{input,activity}', '') IN (
                         'implement_issue',
                         'implement_prompt',
                         'inspect_pr_feedback',
                         'address_pr_feedback'
                     ) THEN 0
                     WHEN COALESCE(data #>> '{input,activity}', '') = 'poll_repo_backlog' THEN 2
                     ELSE 1
                 END ASC,
                 created_at ASC
             LIMIT 1
             FOR UPDATE SKIP LOCKED",
        )
        .fetch_optional(&mut *tx)
        .await?;

        let Some((id, data)) = row else {
            tx.commit().await?;
            return Ok(None);
        };

        let mut job: RuntimeJob = serde_json::from_str(&data)?;
        job.claim(owner, expires_at);
        let updated = to_jsonb_string(&job)?;
        let status = enum_str(&job.status)?;
        sqlx::query(
            "UPDATE runtime_jobs
             SET status = $1, not_before = $2, data = $3::jsonb, updated_at = CURRENT_TIMESTAMP
             WHERE id = $4",
        )
        .bind(&status)
        .bind(job.not_before)
        .bind(&updated)
        .bind(&id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(Some(job))
    }

    pub async fn record_runtime_event(
        &self,
        runtime_job_id: &str,
        event_type: &str,
        payload: Value,
    ) -> anyhow::Result<RuntimeEvent> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
            .bind(format!("runtime_events:{runtime_job_id}"))
            .execute(&mut *tx)
            .await?;
        let (next_sequence,): (i64,) = sqlx::query_as(
            "SELECT COALESCE(MAX(sequence), 0) + 1 FROM runtime_events WHERE runtime_job_id = $1",
        )
        .bind(runtime_job_id)
        .fetch_one(&mut *tx)
        .await?;
        let event = RuntimeEvent::new(runtime_job_id, next_sequence as u64, event_type, payload);
        let data = to_jsonb_string(&event)?;
        sqlx::query(
            "INSERT INTO runtime_events
                (id, runtime_job_id, sequence, event_type, data)
             VALUES ($1, $2, $3, $4, $5::jsonb)",
        )
        .bind(&event.id)
        .bind(&event.runtime_job_id)
        .bind(event.sequence as i64)
        .bind(&event.event_type)
        .bind(&data)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(event)
    }

    pub async fn runtime_events_for(
        &self,
        runtime_job_id: &str,
    ) -> anyhow::Result<Vec<RuntimeEvent>> {
        let rows: Vec<(String,)> = sqlx::query_as(
            "SELECT data::text FROM runtime_events
             WHERE runtime_job_id = $1
             ORDER BY sequence ASC",
        )
        .bind(runtime_job_id)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|(data,)| Ok(serde_json::from_str(&data)?))
            .collect()
    }

    pub async fn runtime_events_for_jobs(
        &self,
        runtime_job_ids: &[String],
    ) -> anyhow::Result<BTreeMap<String, Vec<RuntimeEvent>>> {
        if runtime_job_ids.is_empty() {
            return Ok(BTreeMap::new());
        }
        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT runtime_job_id, data::text FROM runtime_events
             WHERE runtime_job_id = ANY($1::text[])
             ORDER BY runtime_job_id ASC, sequence ASC",
        )
        .bind(runtime_job_ids)
        .fetch_all(&self.pool)
        .await?;
        let mut by_job = BTreeMap::new();
        for (runtime_job_id, data) in rows {
            by_job
                .entry(runtime_job_id)
                .or_insert_with(Vec::new)
                .push(serde_json::from_str(&data)?);
        }
        Ok(by_job)
    }

    pub async fn complete_runtime_job_if_owned(
        &self,
        runtime_job_id: &str,
        owner: &str,
        lease_expires_at: DateTime<Utc>,
        result: &ActivityResult,
    ) -> anyhow::Result<Option<RuntimeJob>> {
        let mut tx = self.pool.begin().await?;
        let row: Option<(String,)> =
            sqlx::query_as("SELECT data::text FROM runtime_jobs WHERE id = $1 FOR UPDATE")
                .bind(runtime_job_id)
                .fetch_optional(&mut *tx)
                .await?;
        let Some((data,)) = row else {
            anyhow::bail!("runtime job not found: {runtime_job_id}");
        };
        let mut job: RuntimeJob = serde_json::from_str(&data)?;
        let is_current_lease = job.status == RuntimeJobStatus::Running
            && job
                .lease
                .as_ref()
                .is_some_and(|lease| lease.owner == owner && lease.expires_at == lease_expires_at);
        if !is_current_lease {
            tx.commit().await?;
            return Ok(None);
        }

        job.complete(result)?;
        let updated = to_jsonb_string(&job)?;
        let status = enum_str(&job.status)?;
        sqlx::query(
            "UPDATE runtime_jobs
             SET status = $1, not_before = $2, data = $3::jsonb, updated_at = CURRENT_TIMESTAMP
             WHERE id = $4",
        )
        .bind(&status)
        .bind(job.not_before)
        .bind(&updated)
        .bind(runtime_job_id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(Some(job))
    }

    pub async fn commit_runtime_activity_completion_if_owned(
        &self,
        runtime_job_id: &str,
        owner: &str,
        lease_expires_at: DateTime<Utc>,
        result: &ActivityResult,
    ) -> anyhow::Result<Option<RuntimeActivityCompletion>> {
        let mut tx = self.pool.begin().await?;
        let command_id_row: Option<(String,)> =
            sqlx::query_as("SELECT command_id FROM runtime_jobs WHERE id = $1")
                .bind(runtime_job_id)
                .fetch_optional(&mut *tx)
                .await?;
        let Some((command_id,)) = command_id_row else {
            anyhow::bail!("runtime job not found: {runtime_job_id}");
        };
        let command_row: Option<WorkflowCommandRecordRow> = sqlx::query_as(
            "SELECT id, workflow_id, decision_id, status, dispatch_owner,
                    dispatch_lease_expires_at, data::text, created_at, updated_at
             FROM workflow_commands
             WHERE id = $1
             FOR UPDATE",
        )
        .bind(&command_id)
        .fetch_optional(&mut *tx)
        .await?;
        let row: Option<(String,)> =
            sqlx::query_as("SELECT data::text FROM runtime_jobs WHERE id = $1 FOR UPDATE")
                .bind(runtime_job_id)
                .fetch_optional(&mut *tx)
                .await?;
        let Some((data,)) = row else {
            anyhow::bail!("runtime job not found: {runtime_job_id}");
        };
        let mut job: RuntimeJob = serde_json::from_str(&data)?;
        let is_current_lease = job.status == RuntimeJobStatus::Running
            && job
                .lease
                .as_ref()
                .is_some_and(|lease| lease.owner == owner && lease.expires_at == lease_expires_at);
        if !is_current_lease {
            tx.commit().await?;
            return Ok(None);
        }

        job.complete(result)?;
        let updated = to_jsonb_string(&job)?;
        let status = enum_str(&job.status)?;
        sqlx::query(
            "UPDATE runtime_jobs
             SET status = $1, not_before = $2, data = $3::jsonb, updated_at = CURRENT_TIMESTAMP
             WHERE id = $4",
        )
        .bind(&status)
        .bind(job.not_before)
        .bind(&updated)
        .bind(runtime_job_id)
        .execute(&mut *tx)
        .await?;
        let Some(command_row) = command_row else {
            tx.commit().await?;
            return Ok(Some(RuntimeActivityCompletion {
                runtime_job: job,
                command: None,
                workflow_event: None,
                decision: None,
            }));
        };
        let mut command = workflow_command_record_from_row(command_row)?;
        let command_status = command_status_for_activity(result.status);
        sqlx::query(
            "UPDATE workflow_commands
             SET status = $1,
                 dispatch_owner = NULL,
                 dispatch_lease_expires_at = NULL,
                 updated_at = CURRENT_TIMESTAMP
             WHERE id = $2",
        )
        .bind(command_status)
        .bind(&command.id)
        .execute(&mut *tx)
        .await?;
        command.status = command_status.to_string();
        command.dispatch_owner = None;
        command.dispatch_lease_expires_at = None;

        let active_start_child_workflow_commands =
            if command.command.command_type == WorkflowCommandType::StartChildWorkflow {
                let command_type = enum_str(&WorkflowCommandType::StartChildWorkflow)?;
                let (count,): (i64,) = sqlx::query_as(
                    "SELECT COUNT(*) FROM workflow_commands
                     WHERE workflow_id = $1
                       AND id <> $2
                       AND command_type = $3
                       AND status IN ('pending', 'dispatching', 'dispatched')",
                )
                .bind(&command.workflow_id)
                .bind(&command.id)
                .bind(&command_type)
                .fetch_one(&mut *tx)
                .await?;
                count as usize
            } else {
                0
            };

        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
            .bind(format!("workflow_events:{}", command.workflow_id))
            .execute(&mut *tx)
            .await?;
        let (next_sequence,): (i64,) = sqlx::query_as(
            "SELECT COALESCE(MAX(sequence), 0) + 1 FROM workflow_events WHERE workflow_id = $1",
        )
        .bind(&command.workflow_id)
        .fetch_one(&mut *tx)
        .await?;
        let event = WorkflowEvent::new(
            &command.workflow_id,
            next_sequence as u64,
            "RuntimeJobCompleted",
            owner,
        )
        .with_payload(json!({
            "command_id": command.id,
            "command": command.command,
            "runtime_job_id": job.id,
            "runtime_job_status": job.status,
            "active_start_child_workflow_commands": active_start_child_workflow_commands,
            "activity_result": result,
        }));
        let event_data = to_jsonb_string(&event)?;
        sqlx::query(
            "INSERT INTO workflow_events
                (id, workflow_id, sequence, event_type, source, data)
             VALUES ($1, $2, $3, $4, $5, $6::jsonb)",
        )
        .bind(&event.id)
        .bind(&event.workflow_id)
        .bind(event.sequence as i64)
        .bind(&event.event_type)
        .bind(&event.source)
        .bind(&event_data)
        .execute(&mut *tx)
        .await?;

        let decision_record = match sqlx::query_as::<_, (String,)>(
            "SELECT data::text FROM workflow_instances WHERE id = $1 FOR UPDATE",
        )
        .bind(&command.workflow_id)
        .fetch_optional(&mut *tx)
        .await?
        {
            Some((instance_data,)) => {
                let mut instance: WorkflowInstance = serde_json::from_str(&instance_data)?;
                match reduce_runtime_job_completed(&instance, &event)? {
                    Some(decision) => {
                        let validator = validator_for_definition(&instance.definition_id);
                        let record = match validator {
                            Some(validator) => {
                                match validator.validate(
                                    &instance,
                                    &decision,
                                    &ValidationContext::new(owner, event.created_at),
                                ) {
                                    Ok(()) => WorkflowDecisionRecord::accepted(
                                        decision.clone(),
                                        Some(event.id.clone()),
                                    ),
                                    Err(error) => WorkflowDecisionRecord::rejected(
                                        decision,
                                        Some(event.id.clone()),
                                        error.to_string(),
                                    ),
                                }
                            }
                            None => WorkflowDecisionRecord::rejected(
                                decision,
                                Some(event.id.clone()),
                                "unknown workflow definition for runtime completion",
                            ),
                        };
                        insert_decision_record_tx(&mut tx, &record).await?;
                        if record.accepted {
                            for followup in &record.decision.commands {
                                let status = if followup.requires_runtime_job() {
                                    "pending"
                                } else {
                                    COMMAND_STATUS_HANDLED_INLINE
                                };
                                insert_workflow_command_tx(
                                    &mut tx,
                                    &instance.id,
                                    Some(&record.id),
                                    followup,
                                    status,
                                )
                                .await?;
                                if !followup.requires_runtime_job() {
                                    apply_inline_command_side_effect(&mut instance, followup)?;
                                }
                            }
                            instance.state = record.decision.next_state.clone();
                            instance.version = instance.version.saturating_add(1);
                            upsert_instance_tx(&mut tx, &instance).await?;
                        }
                        Some(record)
                    }
                    None => None,
                }
            }
            None => None,
        };

        tx.commit().await?;
        Ok(Some(RuntimeActivityCompletion {
            runtime_job: job,
            command: Some(command),
            workflow_event: Some(event),
            decision: decision_record,
        }))
    }

    pub async fn extend_runtime_job_lease_if_owned(
        &self,
        runtime_job_id: &str,
        owner: &str,
        current_lease_expires_at: DateTime<Utc>,
        next_lease_expires_at: DateTime<Utc>,
    ) -> anyhow::Result<Option<RuntimeJob>> {
        let mut tx = self.pool.begin().await?;
        let row: Option<(String,)> =
            sqlx::query_as("SELECT data::text FROM runtime_jobs WHERE id = $1 FOR UPDATE")
                .bind(runtime_job_id)
                .fetch_optional(&mut *tx)
                .await?;
        let Some((data,)) = row else {
            anyhow::bail!("runtime job not found: {runtime_job_id}");
        };
        let mut job: RuntimeJob = serde_json::from_str(&data)?;
        let is_current_lease = job.status == RuntimeJobStatus::Running
            && job.lease.as_ref().is_some_and(|lease| {
                lease.owner == owner && lease.expires_at == current_lease_expires_at
            });
        if !is_current_lease {
            tx.commit().await?;
            return Ok(None);
        }

        job.claim(owner, next_lease_expires_at);
        let updated = to_jsonb_string(&job)?;
        let status = enum_str(&job.status)?;
        sqlx::query(
            "UPDATE runtime_jobs
             SET status = $1, not_before = $2, data = $3::jsonb, updated_at = CURRENT_TIMESTAMP
             WHERE id = $4",
        )
        .bind(&status)
        .bind(job.not_before)
        .bind(&updated)
        .bind(runtime_job_id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(Some(job))
    }

    pub async fn get_runtime_job(
        &self,
        runtime_job_id: &str,
    ) -> anyhow::Result<Option<RuntimeJob>> {
        let row: Option<(String,)> =
            sqlx::query_as("SELECT data::text FROM runtime_jobs WHERE id = $1")
                .bind(runtime_job_id)
                .fetch_optional(&self.pool)
                .await?;
        row.map(|(data,)| serde_json::from_str(&data))
            .transpose()
            .map_err(Into::into)
    }

    pub async fn runtime_jobs_for_command(
        &self,
        command_id: &str,
    ) -> anyhow::Result<Vec<RuntimeJob>> {
        let rows: Vec<(String,)> = sqlx::query_as(
            "SELECT data::text FROM runtime_jobs
             WHERE command_id = $1
             ORDER BY created_at ASC",
        )
        .bind(command_id)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|(data,)| Ok(serde_json::from_str(&data)?))
            .collect()
    }

    pub async fn cancel_command_and_unfinished_runtime_jobs(
        &self,
        command_id: &str,
        activity: &str,
        summary: &str,
    ) -> anyhow::Result<usize> {
        let mut tx = self.pool.begin().await?;
        let _command_row: Option<(String,)> =
            sqlx::query_as("SELECT status FROM workflow_commands WHERE id = $1 FOR UPDATE")
                .bind(command_id)
                .fetch_optional(&mut *tx)
                .await?;
        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT id, data::text FROM runtime_jobs
             WHERE command_id = $1
               AND status IN ('pending', 'running')
             FOR UPDATE",
        )
        .bind(command_id)
        .fetch_all(&mut *tx)
        .await?;
        let mut cancelled = 0usize;
        for (id, data) in rows {
            let mut job: RuntimeJob = serde_json::from_str(&data)?;
            job.complete(&ActivityResult::cancelled(activity, summary))?;
            let updated = to_jsonb_string(&job)?;
            let status = enum_str(&job.status)?;
            sqlx::query(
                "UPDATE runtime_jobs
                 SET status = $1, not_before = $2, data = $3::jsonb, updated_at = CURRENT_TIMESTAMP
                 WHERE id = $4",
            )
            .bind(&status)
            .bind(job.not_before)
            .bind(&updated)
            .bind(&id)
            .execute(&mut *tx)
            .await?;
            cancelled += 1;
        }
        sqlx::query(
            "UPDATE workflow_commands
             SET status = 'cancelled',
                 dispatch_owner = NULL,
                 dispatch_lease_expires_at = NULL,
                 updated_at = CURRENT_TIMESTAMP
             WHERE id = $1
               AND status IN ('pending', 'dispatching', 'dispatched')",
        )
        .bind(command_id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(cancelled)
    }

    pub async fn runtime_jobs_for_commands(
        &self,
        command_ids: &[String],
    ) -> anyhow::Result<BTreeMap<String, Vec<RuntimeJob>>> {
        if command_ids.is_empty() {
            return Ok(BTreeMap::new());
        }
        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT command_id, data::text FROM runtime_jobs
             WHERE command_id = ANY($1::text[])
             ORDER BY command_id ASC, created_at ASC",
        )
        .bind(command_ids)
        .fetch_all(&self.pool)
        .await?;
        let mut by_command = BTreeMap::new();
        for (command_id, data) in rows {
            by_command
                .entry(command_id)
                .or_insert_with(Vec::new)
                .push(serde_json::from_str(&data)?);
        }
        Ok(by_command)
    }

    pub async fn runtime_job_counts_for_commands(
        &self,
        command_ids: &[String],
    ) -> anyhow::Result<BTreeMap<String, usize>> {
        if command_ids.is_empty() {
            return Ok(BTreeMap::new());
        }
        let rows: Vec<(String, i64)> = sqlx::query_as(
            "SELECT command_id, COUNT(*)
             FROM runtime_jobs
             WHERE command_id = ANY($1::text[])
             GROUP BY command_id",
        )
        .bind(command_ids)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|(command_id, count)| (command_id, count.max(0) as usize))
            .collect())
    }

    pub async fn runtime_job_status_counts_for_commands(
        &self,
        command_ids: &[String],
    ) -> anyhow::Result<BTreeMap<String, usize>> {
        if command_ids.is_empty() {
            return Ok(BTreeMap::new());
        }
        let rows: Vec<(String, i64)> = sqlx::query_as(
            "SELECT status, COUNT(*)
             FROM runtime_jobs
             WHERE command_id = ANY($1::text[])
             GROUP BY status
             ORDER BY status ASC",
        )
        .bind(command_ids)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|(status, count)| (status, count.max(0) as usize))
            .collect())
    }

    pub async fn runtime_summary_counts_for_instances(
        &self,
        project_id: Option<&str>,
        activity_envelope_artifact_type: &str,
        activity_envelope_schema: &str,
    ) -> anyhow::Result<WorkflowRuntimeSummaryCounts> {
        let command_status_rows: Vec<(String, i64)> = sqlx::query_as(
            "SELECT command.status, COUNT(*)
             FROM workflow_commands command
             JOIN workflow_instances workflow ON workflow.id = command.workflow_id
             WHERE ($1::text IS NULL OR workflow.data->'data'->>'project_id' = $1)
             GROUP BY command.status
             ORDER BY command.status ASC",
        )
        .bind(project_id)
        .fetch_all(&self.pool)
        .await?;
        let command_statuses: BTreeMap<String, usize> = command_status_rows
            .into_iter()
            .map(|(status, count)| (status, count.max(0) as usize))
            .collect();
        let total_commands = command_statuses.values().sum();

        let runtime_job_status_rows: Vec<(String, i64)> = sqlx::query_as(
            "SELECT job.status, COUNT(*)
             FROM runtime_jobs job
             JOIN workflow_commands command ON command.id = job.command_id
             JOIN workflow_instances workflow ON workflow.id = command.workflow_id
             WHERE ($1::text IS NULL OR workflow.data->'data'->>'project_id' = $1)
             GROUP BY job.status
             ORDER BY job.status ASC",
        )
        .bind(project_id)
        .fetch_all(&self.pool)
        .await?;
        let runtime_job_statuses: BTreeMap<String, usize> = runtime_job_status_rows
            .into_iter()
            .map(|(status, count)| (status, count.max(0) as usize))
            .collect();
        let total_runtime_jobs = runtime_job_statuses.values().sum();

        let activity_outcome_rows: Vec<(Option<String>, i64)> = sqlx::query_as(
            "SELECT envelope.payload->>'outcome' AS outcome, COUNT(*)
             FROM runtime_jobs job
             JOIN workflow_commands command ON command.id = job.command_id
             JOIN workflow_instances workflow ON workflow.id = command.workflow_id
             LEFT JOIN LATERAL (
                 SELECT artifact.value->'artifact' AS payload
                 FROM jsonb_array_elements(
                     CASE
                         WHEN jsonb_typeof(job.data->'output'->'artifacts') = 'array'
                         THEN job.data->'output'->'artifacts'
                         ELSE '[]'::jsonb
                     END
                 ) WITH ORDINALITY AS artifact(value, ordinal)
                 WHERE artifact.value->>'artifact_type' = $2
                   AND artifact.value->'artifact'->>'schema' = $3
                 ORDER BY artifact.ordinal DESC
                 LIMIT 1
             ) envelope ON true
             WHERE ($1::text IS NULL OR workflow.data->'data'->>'project_id' = $1)
             GROUP BY envelope.payload->>'outcome'
             ORDER BY envelope.payload->>'outcome' ASC NULLS FIRST",
        )
        .bind(project_id)
        .bind(activity_envelope_artifact_type)
        .bind(activity_envelope_schema)
        .fetch_all(&self.pool)
        .await?;
        let mut activity_outcomes = BTreeMap::new();
        let mut jobs_without_activity_envelope = 0;
        for (outcome, count) in activity_outcome_rows {
            match outcome {
                Some(outcome) => {
                    activity_outcomes.insert(outcome, count.max(0) as usize);
                }
                None => {
                    jobs_without_activity_envelope = count.max(0) as usize;
                }
            }
        }

        Ok(WorkflowRuntimeSummaryCounts {
            total_commands,
            total_runtime_jobs,
            command_statuses,
            runtime_job_statuses,
            activity_outcomes,
            jobs_without_activity_envelope,
        })
    }

    pub async fn runtime_jobs_for_commands_limited(
        &self,
        command_ids: &[String],
        per_command_limit: i64,
    ) -> anyhow::Result<BTreeMap<String, Vec<RuntimeJob>>> {
        if command_ids.is_empty() || per_command_limit <= 0 {
            return Ok(BTreeMap::new());
        }
        let per_command_limit = per_command_limit.clamp(1, 50);
        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT job.command_id, job.data
             FROM unnest($1::text[]) AS selected(command_id)
             JOIN LATERAL (
                 SELECT command_id, data::text AS data, created_at, id
                 FROM runtime_jobs
                 WHERE command_id = selected.command_id
                 ORDER BY created_at DESC, id DESC
                 LIMIT $2
             ) AS job ON true
             ORDER BY job.command_id ASC, job.created_at ASC, job.id ASC",
        )
        .bind(command_ids)
        .bind(per_command_limit)
        .fetch_all(&self.pool)
        .await?;
        let mut by_command = BTreeMap::new();
        for (command_id, data) in rows {
            by_command
                .entry(command_id)
                .or_insert_with(Vec::new)
                .push(serde_json::from_str(&data)?);
        }
        Ok(by_command)
    }

    pub async fn runtime_turns_started_for_workflow(
        &self,
        workflow_id: &str,
        exclude_runtime_job_id: Option<&str>,
    ) -> anyhow::Result<i64> {
        let (count,): (i64,) = sqlx::query_as(
            "SELECT COUNT(*)
             FROM runtime_events e
             JOIN runtime_jobs r ON r.id = e.runtime_job_id
             JOIN workflow_commands c ON c.id = r.command_id
             WHERE c.workflow_id = $1
               AND e.event_type = 'RuntimeTurnStarted'
               AND ($2::text IS NULL OR r.id <> $2)",
        )
        .bind(workflow_id)
        .bind(exclude_runtime_job_id)
        .fetch_one(&self.pool)
        .await?;
        Ok(count)
    }

    pub async fn runtime_job_count_by_status(
        &self,
        status: RuntimeJobStatus,
    ) -> anyhow::Result<i64> {
        let status = enum_str(&status)?;
        let (count,): (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM runtime_jobs WHERE status = $1")
                .bind(&status)
                .fetch_one(&self.pool)
                .await?;
        Ok(count)
    }
}

pub(super) fn to_jsonb_string(value: &impl Serialize) -> anyhow::Result<String> {
    Ok(serde_json::to_string(value)?.replace("\\u0000", ""))
}

pub(super) fn enum_str(value: &impl Serialize) -> anyhow::Result<String> {
    serde_json::to_value(value)?
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| anyhow::anyhow!("serialized enum did not produce a string"))
}

async fn runtime_job_for_command_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    command_id: &str,
) -> anyhow::Result<Option<RuntimeJob>> {
    let row: Option<(String,)> = sqlx::query_as(
        "SELECT data::text FROM runtime_jobs
         WHERE command_id = $1
         ORDER BY created_at ASC
         LIMIT 1",
    )
    .bind(command_id)
    .fetch_optional(&mut **tx)
    .await?;
    row.map(|(data,)| serde_json::from_str(&data))
        .transpose()
        .map_err(Into::into)
}

async fn insert_decision_record_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    record: &WorkflowDecisionRecord,
) -> anyhow::Result<()> {
    let data = to_jsonb_string(record)?;
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
    .bind(&data)
    .bind(&record.rejection_reason)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

async fn insert_workflow_command_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workflow_id: &str,
    decision_id: Option<&str>,
    command: &WorkflowCommand,
    status: &str,
) -> anyhow::Result<String> {
    let data = to_jsonb_string(command)?;
    let command_type = enum_str(&command.command_type)?;
    let (id,): (String,) = sqlx::query_as(
        "INSERT INTO workflow_commands
            (id, workflow_id, decision_id, command_type, dedupe_key, status, data)
         VALUES ($1, $2, $3, $4, $5, $6, $7::jsonb)
         ON CONFLICT (workflow_id, dedupe_key) DO UPDATE SET
            status = CASE
                WHEN workflow_commands.status = 'pending' THEN EXCLUDED.status
                ELSE workflow_commands.status
            END,
            updated_at = CASE
                WHEN workflow_commands.status = 'pending'
                     AND workflow_commands.status <> EXCLUDED.status
                THEN CURRENT_TIMESTAMP
                ELSE workflow_commands.updated_at
            END
         RETURNING id",
    )
    .bind(Uuid::new_v4().to_string())
    .bind(workflow_id)
    .bind(decision_id)
    .bind(&command_type)
    .bind(&command.dedupe_key)
    .bind(status)
    .bind(&data)
    .fetch_one(&mut **tx)
    .await?;
    Ok(id)
}

async fn load_or_insert_initial_instance_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workflow_id: &str,
    expected_state: &str,
    create_if_missing: Option<&WorkflowInstance>,
) -> anyhow::Result<Option<WorkflowInstance>> {
    if let Some(instance) = select_instance_for_update_tx(tx, workflow_id).await? {
        return Ok(Some(instance));
    }

    let Some(initial_instance) = create_if_missing else {
        return Ok(None);
    };
    if initial_instance.id != workflow_id {
        anyhow::bail!(
            "initial workflow instance `{}` does not match workflow `{}`",
            initial_instance.id,
            workflow_id
        );
    }
    if initial_instance.state != expected_state {
        return Ok(None);
    }

    if insert_instance_if_absent_tx(tx, initial_instance).await? {
        return Ok(Some(initial_instance.clone()));
    }

    select_instance_for_update_tx(tx, workflow_id).await
}

async fn select_instance_for_update_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workflow_id: &str,
) -> anyhow::Result<Option<WorkflowInstance>> {
    let row: Option<(String,)> =
        sqlx::query_as("SELECT data::text FROM workflow_instances WHERE id = $1 FOR UPDATE")
            .bind(workflow_id)
            .fetch_optional(&mut **tx)
            .await?;
    row.map(|(data,)| serde_json::from_str(&data))
        .transpose()
        .map_err(Into::into)
}

async fn insert_event_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workflow_id: &str,
    event_type: &str,
    source: &str,
    payload: Value,
) -> anyhow::Result<WorkflowEvent> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(format!("workflow_events:{workflow_id}"))
        .execute(&mut **tx)
        .await?;
    let (next_sequence,): (i64,) = sqlx::query_as(
        "SELECT COALESCE(MAX(sequence), 0) + 1 FROM workflow_events WHERE workflow_id = $1",
    )
    .bind(workflow_id)
    .fetch_one(&mut **tx)
    .await?;
    let event = WorkflowEvent::new(workflow_id, next_sequence as u64, event_type, source)
        .with_payload(payload);
    let event_data = to_jsonb_string(&event)?;
    sqlx::query(
        "INSERT INTO workflow_events
            (id, workflow_id, sequence, event_type, source, data)
         VALUES ($1, $2, $3, $4, $5, $6::jsonb)",
    )
    .bind(&event.id)
    .bind(&event.workflow_id)
    .bind(event.sequence as i64)
    .bind(&event.event_type)
    .bind(&event.source)
    .bind(&event_data)
    .execute(&mut **tx)
    .await?;
    Ok(event)
}

async fn insert_instance_if_absent_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    instance: &WorkflowInstance,
) -> anyhow::Result<bool> {
    let data = to_jsonb_string(instance)?;
    let result = sqlx::query(
        "INSERT INTO workflow_instances
            (id, definition_id, state, subject_type, subject_key, parent_workflow_id, data, version)
         VALUES ($1, $2, $3, $4, $5, $6, $7::jsonb, $8)
         ON CONFLICT (id) DO NOTHING",
    )
    .bind(&instance.id)
    .bind(&instance.definition_id)
    .bind(&instance.state)
    .bind(&instance.subject.subject_type)
    .bind(&instance.subject.subject_key)
    .bind(&instance.parent_workflow_id)
    .bind(&data)
    .bind(instance.version as i64)
    .execute(&mut **tx)
    .await?;
    Ok(result.rows_affected() == 1)
}

async fn upsert_instance_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    instance: &WorkflowInstance,
) -> anyhow::Result<()> {
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
    .execute(&mut **tx)
    .await?;
    Ok(())
}

fn apply_inline_command_side_effect(
    instance: &mut WorkflowInstance,
    command: &WorkflowCommand,
) -> anyhow::Result<()> {
    match command.command_type {
        WorkflowCommandType::BindPr => apply_bind_pr_side_effect(instance, command),
        WorkflowCommandType::MarkDone => apply_mark_done_side_effect(instance, command),
        _ => Ok(()),
    }
}

fn apply_bind_pr_side_effect(
    instance: &mut WorkflowInstance,
    command: &WorkflowCommand,
) -> anyhow::Result<()> {
    let pr_number = command
        .command
        .get("pr_number")
        .and_then(Value::as_u64)
        .context("bind_pr command missing pr_number")?;
    let pr_url = command
        .command
        .get("pr_url")
        .and_then(Value::as_str)
        .context("bind_pr command missing pr_url")?;

    if !instance.data.is_object() {
        instance.data = json!({});
    }
    let data = instance
        .data
        .as_object_mut()
        .context("workflow instance data is not an object")?;
    data.insert("pr_number".to_string(), json!(pr_number));
    data.insert("pr_url".to_string(), json!(pr_url));
    Ok(())
}

fn apply_mark_done_side_effect(
    instance: &mut WorkflowInstance,
    command: &WorkflowCommand,
) -> anyhow::Result<()> {
    let Some(closed_issue_evidence) = command.command.get("closed_issue_evidence").cloned() else {
        return Ok(());
    };
    if !instance.data.is_object() {
        instance.data = json!({});
    }
    let data = instance
        .data
        .as_object_mut()
        .context("workflow instance data is not an object")?;
    data.insert("closed_issue_evidence".to_string(), closed_issue_evidence);
    Ok(())
}

fn command_status_for_activity(status: ActivityStatus) -> &'static str {
    match status {
        ActivityStatus::Succeeded => "completed",
        ActivityStatus::Failed => "failed",
        ActivityStatus::Blocked => "blocked",
        ActivityStatus::Cancelled => "cancelled",
    }
}

fn validator_for_definition(definition_id: &str) -> Option<DecisionValidator> {
    match definition_id {
        GITHUB_ISSUE_PR_DEFINITION_ID => Some(DecisionValidator::github_issue_pr()),
        QUALITY_GATE_DEFINITION_ID => Some(DecisionValidator::quality_gate()),
        PR_FEEDBACK_DEFINITION_ID => Some(DecisionValidator::pr_feedback()),
        PROMPT_TASK_DEFINITION_ID => Some(DecisionValidator::prompt_task()),
        REPO_BACKLOG_DEFINITION_ID => Some(DecisionValidator::repo_backlog()),
        _ => None,
    }
}
