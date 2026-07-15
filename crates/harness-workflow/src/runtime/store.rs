use super::command_record::{
    from_row as workflow_command_record_from_row, WorkflowCommandRecordRow,
};
use super::errors::RuntimeJobNotFoundError;
use super::model::{
    ActivityResult, ActivityStatus, RuntimeEvent, RuntimeJob, RuntimeJobStatus, RuntimeKind,
    WorkflowCommand, WorkflowCommandRecord, WorkflowCommandType, WorkflowDecision,
    WorkflowDecisionRecord, WorkflowEvent, WorkflowInstance, WorkflowLease,
};
use super::status::WorkflowCommandStatus;
use super::store_migrations::WORKFLOW_RUNTIME_MIGRATIONS;
use super::validator::DecisionValidator;
use anyhow::Context;
use chrono::{DateTime, Utc};
use harness_core::db::PgStoreContext;
use serde::{de::DeserializeOwned, Serialize};
use serde_json::{json, Value};
use sqlx::postgres::PgPool;
use std::collections::BTreeMap;
use std::path::Path;

#[path = "store/commands.rs"]
mod command_store;
#[path = "store/definitions.rs"]
mod definitions;
#[path = "store/driverless_progress.rs"]
mod driverless_progress;
#[path = "store/instance_helpers.rs"]
mod instance_helpers;
#[path = "store/instances.rs"]
mod instances;
#[path = "store/recovery.rs"]
mod recovery;
#[path = "store/runtime_completion.rs"]
mod runtime_completion;
#[path = "store/runtime_job_leases.rs"]
pub mod runtime_job_leases;
#[path = "store/runtime_usage.rs"]
mod runtime_usage;
#[path = "store/submission_commit.rs"]
mod submission_commit;
pub use driverless_progress::{DriverlessProgressInstance, DriverlessProgressProvenanceStatus};
pub use recovery::{
    WorkflowRuntimeRecoveryAction, WorkflowRuntimeRecoveryOutcome, WorkflowRuntimeRecoveryRequest,
};
pub use runtime_usage::{
    RuntimeUsageMetrics, RuntimeUsageRecord, RuntimeUsageUpsert, RuntimeUsageUpsertOutcome,
};
pub use submission_commit::{
    WorkflowSubmissionDecisionCommit, WorkflowSubmissionDecisionTransition,
    WorkflowSubmissionPromptPayload,
};
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
pub struct WorkflowRuntimeDetailCounts {
    pub event_count: usize,
    pub decision_count: usize,
    pub rejected_decision_count: usize,
    pub command_count: usize,
    pub runtime_job_count: usize,
}
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct WorkflowRuntimeSummaryCounts {
    pub workflow_states: Vec<WorkflowRuntimeStateCount>,
    pub total_commands: usize,
    pub total_runtime_jobs: usize,
    pub command_statuses: BTreeMap<String, usize>,
    pub runtime_job_statuses: BTreeMap<String, usize>,
    pub running_job_lease_statuses: BTreeMap<String, usize>,
    pub activity_outcomes: BTreeMap<String, usize>,
    pub jobs_without_activity_envelope: usize,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowRuntimeStateCount {
    pub definition_id: String,
    pub state: String,
    pub count: usize,
}
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct RuntimeHistoryPruneSummary {
    pub workflow_instances_deleted: usize,
    pub workflow_events_deleted: usize,
    pub workflow_decisions_deleted: usize,
    pub workflow_commands_deleted: usize,
    pub runtime_jobs_deleted: usize,
    pub runtime_events_deleted: usize,
    pub workflow_artifacts_deleted: usize,
}

impl RuntimeHistoryPruneSummary {
    pub fn is_empty(&self) -> bool {
        self.workflow_instances_deleted == 0
            && self.workflow_events_deleted == 0
            && self.workflow_decisions_deleted == 0
            && self.workflow_commands_deleted == 0
            && self.runtime_jobs_deleted == 0
            && self.runtime_events_deleted == 0
            && self.workflow_artifacts_deleted == 0
    }
}
#[derive(Debug, Clone, PartialEq)]
pub struct RuntimeJobCompactRecord {
    pub id: String,
    pub command_id: String,
    pub runtime_kind: RuntimeKind,
    pub runtime_profile: String,
    pub status: RuntimeJobStatus,
    pub lease: Option<WorkflowLease>,
    pub lease_generation: u64,
    pub error: Option<String>,
    pub failure_class: Option<String>,
    pub not_before: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct RuntimeEventSummary {
    pub runtime_job_id: String,
    pub runtime_event_count: usize,
    pub latest_runtime_event_type: Option<String>,
    pub prompt_packet_digest: Option<String>,
    pub latest_runtime_event_at: Option<DateTime<Utc>>,
    pub latest_turn_sequence: Option<u64>,
    pub latest_activity_result_sequence: Option<u64>,
}
#[derive(Debug, Clone, PartialEq)]
pub struct RuntimeJobCommandSource {
    pub runtime_job_id: String,
    pub workflow_id: String,
    pub command_id: String,
    pub command: WorkflowCommand,
}
pub struct WorkflowDecisionTransition<'a> {
    pub expected_state: &'a str,
    pub create_if_missing: Option<&'a WorkflowInstance>,
    pub event_type: &'a str,
    pub source: &'a str,
    pub payload: Value,
    pub decision: &'a WorkflowDecision,
    pub final_instance: &'a WorkflowInstance,
    pub command_status: WorkflowCommandStatus,
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
type RuntimeEventSummaryRow = (
    String,
    i64,
    Option<String>,
    Option<DateTime<Utc>>,
    Option<String>,
    Option<i64>,
    Option<i64>,
);
type RuntimeJobCompactRecordRow = (
    String,
    String,
    String,
    String,
    String,
    Option<String>,
    i64,
    Option<String>,
    Option<String>,
    Option<DateTime<Utc>>,
    DateTime<Utc>,
    DateTime<Utc>,
);
#[derive(Debug, Clone, PartialEq)]
pub enum RuntimeJobEnqueueOutcome {
    Enqueued(RuntimeJob),
    AlreadyExists(RuntimeJob),
    StaleClaim,
    WorkflowTerminal { status: WorkflowCommandStatus },
    CommandNotPending { status: WorkflowCommandStatus },
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClaimedCommandTerminalOutcome {
    NotTerminal,
    StaleClaim,
    WorkflowTerminal {
        status: WorkflowCommandStatus,
        workflow_state: String,
    },
}
#[derive(Debug, Clone, PartialEq)]
pub struct RuntimeActivityCompletion {
    pub runtime_job: RuntimeJob,
    pub command: Option<WorkflowCommandRecord>,
    pub workflow_event: Option<WorkflowEvent>,
    pub decision: Option<WorkflowDecisionRecord>,
}
fn workflow_instance_from_row(
    data: String,
    updated_at: DateTime<Utc>,
) -> anyhow::Result<WorkflowInstance> {
    let mut instance: WorkflowInstance = serde_json::from_str(&data)?;
    instance.updated_at = updated_at;
    Ok(instance)
}

impl WorkflowRuntimeStore {
    pub async fn open(path: &Path) -> anyhow::Result<Self> {
        Self::open_with_database_url(path, None).await
    }
    pub async fn open_with_database_url(
        path: &Path,
        configured_database_url: Option<&str>,
    ) -> anyhow::Result<Self> {
        let context = PgStoreContext::from_legacy_path_schema(path, configured_database_url)?;
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
    pub fn pool(&self) -> &PgPool {
        &self.pool
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

    pub async fn detail_counts_for_workflows(
        &self,
        workflow_ids: &[String],
    ) -> anyhow::Result<BTreeMap<String, WorkflowRuntimeDetailCounts>> {
        if workflow_ids.is_empty() {
            return Ok(BTreeMap::new());
        }
        let rows: Vec<(String, i64, i64, i64, i64, i64)> = sqlx::query_as(
            "SELECT selected.workflow_id,
                    COALESCE(events.event_count, 0),
                    COALESCE(decisions.decision_count, 0),
                    COALESCE(decisions.rejected_decision_count, 0),
                    COALESCE(commands.command_count, 0),
                    COALESCE(jobs.runtime_job_count, 0)
             FROM unnest($1::text[]) AS selected(workflow_id)
             LEFT JOIN LATERAL (
                 SELECT COUNT(*) AS event_count
                 FROM workflow_events
                 WHERE workflow_id = selected.workflow_id
             ) AS events ON true
             LEFT JOIN LATERAL (
                 SELECT COUNT(*) AS decision_count,
                        COUNT(*) FILTER (WHERE accepted = false) AS rejected_decision_count
                 FROM workflow_decisions
                 WHERE workflow_id = selected.workflow_id
             ) AS decisions ON true
             LEFT JOIN LATERAL (
                 SELECT COUNT(*) AS command_count
                 FROM workflow_commands
                 WHERE workflow_id = selected.workflow_id
             ) AS commands ON true
             LEFT JOIN LATERAL (
                 SELECT COUNT(runtime_jobs.id) AS runtime_job_count
                 FROM workflow_commands
                 JOIN runtime_jobs ON runtime_jobs.command_id = workflow_commands.id
                 WHERE workflow_commands.workflow_id = selected.workflow_id
             ) AS jobs ON true",
        )
        .bind(workflow_ids)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(
                |(
                    workflow_id,
                    event_count,
                    decision_count,
                    rejected_decision_count,
                    command_count,
                    runtime_job_count,
                )| {
                    (
                        workflow_id,
                        WorkflowRuntimeDetailCounts {
                            event_count: event_count.max(0) as usize,
                            decision_count: decision_count.max(0) as usize,
                            rejected_decision_count: rejected_decision_count.max(0) as usize,
                            command_count: command_count.max(0) as usize,
                            runtime_job_count: runtime_job_count.max(0) as usize,
                        },
                    )
                },
            )
            .collect())
    }

    pub async fn rejected_decisions_for_workflows_limited(
        &self,
        workflow_ids: &[String],
        per_workflow_limit: i64,
    ) -> anyhow::Result<BTreeMap<String, Vec<WorkflowDecisionRecord>>> {
        if workflow_ids.is_empty() || per_workflow_limit <= 0 {
            return Ok(BTreeMap::new());
        }
        let per_workflow_limit = per_workflow_limit.clamp(1, 20);
        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT decision.workflow_id, decision.data
             FROM unnest($1::text[]) AS selected(workflow_id)
             JOIN LATERAL (
                 SELECT workflow_id, data::text AS data, created_at
                 FROM workflow_decisions
                 WHERE workflow_id = selected.workflow_id
                   AND accepted = false
                 ORDER BY created_at DESC
                 LIMIT $2
             ) AS decision ON true
             ORDER BY decision.workflow_id ASC, decision.created_at DESC",
        )
        .bind(workflow_ids)
        .bind(per_workflow_limit)
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
        self.enqueue_command_with_status(
            workflow_id,
            decision_id,
            command,
            WorkflowCommandStatus::Pending,
        )
        .await
    }

    pub async fn enqueue_command_with_status(
        &self,
        workflow_id: &str,
        decision_id: Option<&str>,
        command: &WorkflowCommand,
        status: WorkflowCommandStatus,
    ) -> anyhow::Result<String> {
        command_store::insert(&self.pool, workflow_id, decision_id, command, status).await
    }

    pub async fn commands_for(
        &self,
        workflow_id: &str,
    ) -> anyhow::Result<Vec<WorkflowCommandRecord>> {
        let rows: Vec<WorkflowCommandRecordRow> = sqlx::query_as(
            "SELECT id, workflow_id, decision_id, status, dispatch_owner,
                    dispatch_lease_expires_at, dispatch_not_before,
                    dispatch_attempt_count, dispatch_claim_generation,
                    dispatch_barrier::text, data::text, created_at, updated_at
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
                    dispatch_lease_expires_at, dispatch_not_before,
                    dispatch_attempt_count, dispatch_claim_generation,
                    dispatch_barrier::text, data::text, created_at, updated_at
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

    pub async fn commands_for_workflows_limited(
        &self,
        workflow_ids: &[String],
        per_workflow_limit: i64,
    ) -> anyhow::Result<BTreeMap<String, Vec<WorkflowCommandRecord>>> {
        if workflow_ids.is_empty() || per_workflow_limit <= 0 {
            return Ok(BTreeMap::new());
        }
        let per_workflow_limit = per_workflow_limit.clamp(1, 50);
        let rows: Vec<WorkflowCommandRecordRow> = sqlx::query_as(
            "SELECT command.id, command.workflow_id, command.decision_id, command.status,
                    command.dispatch_owner, command.dispatch_lease_expires_at,
                    command.dispatch_not_before, command.dispatch_attempt_count,
                    command.dispatch_claim_generation, command.dispatch_barrier,
                    command.data,
                    command.created_at, command.updated_at
             FROM unnest($1::text[]) AS selected(workflow_id)
             JOIN LATERAL (
                 SELECT id, workflow_id, decision_id, status, dispatch_owner,
                        dispatch_lease_expires_at, dispatch_not_before,
                        dispatch_attempt_count, dispatch_claim_generation,
                        dispatch_barrier::text AS dispatch_barrier,
                        data::text AS data, created_at, updated_at
                 FROM workflow_commands
                 WHERE workflow_id = selected.workflow_id
                 ORDER BY created_at DESC
                 LIMIT $2
             ) AS command ON true
             ORDER BY command.workflow_id ASC, command.created_at ASC",
        )
        .bind(workflow_ids)
        .bind(per_workflow_limit)
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
                    dispatch_lease_expires_at, dispatch_not_before,
                    dispatch_attempt_count, dispatch_claim_generation,
                    dispatch_barrier::text, data::text, created_at, updated_at
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
            "SELECT command.id, command.workflow_id, command.decision_id, command.status,
                    command.dispatch_owner, command.dispatch_lease_expires_at,
                    command.dispatch_not_before, command.dispatch_attempt_count,
                    command.dispatch_claim_generation, command.dispatch_barrier::text,
                    command.data::text, command.created_at, command.updated_at
             FROM workflow_commands AS command
             JOIN workflow_instances AS workflow ON workflow.id = command.workflow_id
             WHERE command.status = 'pending'
             ORDER BY command.created_at ASC
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
        let mut tx = self.pool.begin().await?;
        let rows: Vec<WorkflowCommandRecordRow> = sqlx::query_as(
            "WITH candidates AS (
                 SELECT command.id
                 FROM workflow_commands AS command
                 JOIN workflow_instances AS workflow ON workflow.id = command.workflow_id
             WHERE command.status = $3
                    OR (
                        command.status = $4
                        AND COALESCE(command.dispatch_lease_expires_at, '-infinity'::timestamptz)
                            <= CURRENT_TIMESTAMP
                    )
                    OR (
                        command.status = $5
                        AND command.dispatch_not_before <= CURRENT_TIMESTAMP
                        AND command.dispatch_owner IS NULL
                        AND command.dispatch_lease_expires_at IS NULL
                        AND command.dispatch_attempt_count > 0
                        AND command.dispatch_claim_generation > 0
                        AND command.dispatch_barrier IS NOT NULL
                        AND jsonb_typeof(command.dispatch_barrier) = 'object'
                        AND NULLIF(BTRIM(command.dispatch_barrier->>'reason'), '') IS NOT NULL
                        AND NULLIF(BTRIM(command.dispatch_barrier->>'project_id'), '') IS NOT NULL
                        AND NULLIF(BTRIM(command.dispatch_barrier->>'dispatch_owner'), '') IS NOT NULL
                        AND command.dispatch_barrier->>'command_id' = command.id
                        AND command.dispatch_barrier->>'workflow_id' = command.workflow_id
                        AND (command.dispatch_barrier->>'attempt')::BIGINT
                            = command.dispatch_attempt_count
                        AND (command.dispatch_barrier->>'claim_generation')::BIGINT
                            = command.dispatch_claim_generation
                        AND (command.dispatch_barrier->>'next_dispatch_at')::TIMESTAMPTZ
                            = command.dispatch_not_before
                    )
                 ORDER BY command.created_at ASC
                 LIMIT $6
                 FOR UPDATE OF command SKIP LOCKED
             )
             UPDATE workflow_commands AS command
             SET status = $4,
                 dispatch_owner = $1,
                 dispatch_lease_expires_at = $2,
                 dispatch_claim_generation = command.dispatch_claim_generation + 1,
                 updated_at = CURRENT_TIMESTAMP
             FROM candidates
             WHERE command.id = candidates.id
             RETURNING command.id, command.workflow_id, command.decision_id, command.status,
                       command.dispatch_owner, command.dispatch_lease_expires_at,
                       command.dispatch_not_before, command.dispatch_attempt_count,
                       command.dispatch_claim_generation, command.dispatch_barrier::text,
                       command.data::text, command.created_at, command.updated_at",
        )
        .bind(owner)
        .bind(expires_at)
        .bind(WorkflowCommandStatus::Pending.as_str())
        .bind(WorkflowCommandStatus::Dispatching.as_str())
        .bind(WorkflowCommandStatus::Deferred.as_str())
        .bind(limit)
        .fetch_all(&mut *tx)
        .await?;
        let records: anyhow::Result<Vec<_>> = rows
            .into_iter()
            .map(workflow_command_record_from_row)
            .collect();
        let mut records = match records {
            Ok(records) => records,
            Err(error) => {
                tx.rollback().await?;
                return Err(error);
            }
        };
        tx.commit().await?;
        records.sort_by_key(|record| record.created_at);
        Ok(records)
    }

    pub async fn mark_command_status(
        &self,
        command_id: &str,
        status: WorkflowCommandStatus,
    ) -> anyhow::Result<()> {
        sqlx::query(
            "UPDATE workflow_commands
             SET status = $1,
                 dispatch_owner = NULL,
                 dispatch_lease_expires_at = NULL,
                 dispatch_not_before = NULL,
                 dispatch_barrier = NULL,
                 updated_at = CURRENT_TIMESTAMP
             WHERE id = $2",
        )
        .bind(status.as_str())
        .bind(command_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn mark_pending_command_status(
        &self,
        command_id: &str,
        status: WorkflowCommandStatus,
    ) -> anyhow::Result<bool> {
        let result = sqlx::query(
            "UPDATE workflow_commands
             SET status = $1,
                 dispatch_owner = NULL,
                 dispatch_lease_expires_at = NULL,
                 updated_at = CURRENT_TIMESTAMP
             WHERE id = $2 AND status = $3",
        )
        .bind(status.as_str())
        .bind(command_id)
        .bind(WorkflowCommandStatus::Pending.as_str())
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
        command_store::enqueue_runtime_job_for_command(
            &self.pool,
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
        dispatch_claim: super::DispatchClaim<'_>,
        runtime_kind: RuntimeKind,
        runtime_profile: &str,
        input: Value,
        not_before: Option<DateTime<Utc>>,
    ) -> anyhow::Result<RuntimeJobEnqueueOutcome> {
        command_store::enqueue_runtime_job_for_command(
            &self.pool,
            command_id,
            Some(dispatch_claim),
            runtime_kind,
            runtime_profile,
            input,
            not_before,
        )
        .await
    }

    pub async fn claim_next_runtime_job(
        &self,
        owner: &str,
        expires_at: chrono::DateTime<chrono::Utc>,
    ) -> anyhow::Result<Option<RuntimeJob>> {
        let mut tx = self.pool.begin().await?;
        let row: Option<(String, String)> = sqlx::query_as(
            "SELECT job.id, job.data::text
             FROM runtime_jobs AS job
             JOIN workflow_commands AS command ON command.id = job.command_id
             JOIN workflow_instances AS workflow ON workflow.id = command.workflow_id
             WHERE (
                 job.status = 'pending'
                 AND (job.not_before IS NULL OR job.not_before <= CURRENT_TIMESTAMP)
             ) OR (
                 job.status = 'running'
                 AND job.data ? 'lease'
                 AND (job.data->'lease' ? 'expires_at')
                 AND (job.data->'lease'->>'expires_at')::timestamptz <= CURRENT_TIMESTAMP
             )
             ORDER BY
                 CASE
                     WHEN COALESCE(job.data #>> '{input,activity}', '') IN (
                         'implement_issue',
                         'implement_prompt',
                         'inspect_pr_feedback',
                         'address_pr_feedback'
                     ) THEN 0
                     ELSE 1
                 END ASC,
                 job.created_at ASC
             LIMIT 1
             FOR UPDATE OF job SKIP LOCKED",
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
        let event = runtime_job_leases::append_runtime_event_tx(
            &mut tx,
            runtime_job_id,
            event_type,
            payload,
        )
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

    pub async fn runtime_event_summaries_for_jobs(
        &self,
        runtime_job_ids: &[String],
    ) -> anyhow::Result<BTreeMap<String, RuntimeEventSummary>> {
        if runtime_job_ids.is_empty() {
            return Ok(BTreeMap::new());
        }
        let rows: Vec<RuntimeEventSummaryRow> = sqlx::query_as(
            "SELECT selected.runtime_job_id,
                    COALESCE(counts.runtime_event_count, 0),
                    latest.event_type,
                    latest.created_at,
                    prompt.prompt_packet_digest,
                    counts.latest_turn_sequence,
                    counts.latest_activity_result_sequence
             FROM unnest($1::text[]) AS selected(runtime_job_id)
             LEFT JOIN LATERAL (
                 SELECT COUNT(*) AS runtime_event_count,
                        MAX(sequence) FILTER (WHERE event_type = 'RuntimeTurnStarted')
                            AS latest_turn_sequence,
                        MAX(sequence) FILTER (WHERE event_type = 'ActivityResultReady')
                            AS latest_activity_result_sequence
                 FROM runtime_events
                 WHERE runtime_job_id = selected.runtime_job_id
             ) AS counts ON true
             LEFT JOIN LATERAL (
                 SELECT event_type, created_at
                 FROM runtime_events
                 WHERE runtime_job_id = selected.runtime_job_id
                 ORDER BY sequence DESC
                 LIMIT 1
             ) AS latest ON true
             LEFT JOIN LATERAL (
                 SELECT data #>> '{event,prompt_packet_digest}' AS prompt_packet_digest
                 FROM runtime_events
                 WHERE runtime_job_id = selected.runtime_job_id
                   AND event_type = 'RuntimePromptPrepared'
                   AND data #>> '{event,prompt_packet_digest}' IS NOT NULL
                 ORDER BY sequence DESC
                 LIMIT 1
             ) AS prompt ON true",
        )
        .bind(runtime_job_ids)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(
                |(
                    runtime_job_id,
                    runtime_event_count,
                    latest_runtime_event_type,
                    latest_runtime_event_at,
                    prompt_packet_digest,
                    latest_turn_sequence,
                    latest_activity_result_sequence,
                )| {
                    (
                        runtime_job_id.clone(),
                        RuntimeEventSummary {
                            runtime_job_id,
                            runtime_event_count: runtime_event_count.max(0) as usize,
                            latest_runtime_event_type,
                            prompt_packet_digest,
                            latest_runtime_event_at,
                            latest_turn_sequence: latest_turn_sequence
                                .and_then(|sequence| u64::try_from(sequence).ok()),
                            latest_activity_result_sequence: latest_activity_result_sequence
                                .and_then(|sequence| u64::try_from(sequence).ok()),
                        },
                    )
                },
            )
            .collect())
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
            return Err(RuntimeJobNotFoundError::new(runtime_job_id).into());
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
        runtime_job_leases::delete_runtime_job_lease_receipts_tx(
            &mut tx,
            runtime_job_id,
            job.lease_generation,
        )
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
        self.commit_runtime_activity_completion_if_owned_with_generation(
            runtime_job_id,
            owner,
            lease_expires_at,
            None,
            result,
        )
        .await
    }

    pub async fn commit_runtime_activity_completion_if_owned_with_generation(
        &self,
        runtime_job_id: &str,
        owner: &str,
        lease_expires_at: DateTime<Utc>,
        lease_generation: Option<u64>,
        result: &ActivityResult,
    ) -> anyhow::Result<Option<RuntimeActivityCompletion>> {
        let mut tx = self.pool.begin().await?;
        let command_id_row: Option<(String,)> =
            sqlx::query_as("SELECT command_id FROM runtime_jobs WHERE id = $1")
                .bind(runtime_job_id)
                .fetch_optional(&mut *tx)
                .await?;
        let Some((command_id,)) = command_id_row else {
            return Err(RuntimeJobNotFoundError::new(runtime_job_id).into());
        };
        let command_row: Option<WorkflowCommandRecordRow> = sqlx::query_as(
            "SELECT id, workflow_id, decision_id, status, dispatch_owner,
                    dispatch_lease_expires_at, dispatch_not_before,
                    dispatch_attempt_count, dispatch_claim_generation,
                    dispatch_barrier::text, data::text, created_at, updated_at
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
            return Err(RuntimeJobNotFoundError::new(runtime_job_id).into());
        };
        let mut job: RuntimeJob = serde_json::from_str(&data)?;
        let is_current_lease = job.status == RuntimeJobStatus::Running
            && lease_generation.is_none_or(|generation| generation == job.lease_generation)
            && job.lease.as_ref().is_some_and(|lease| {
                lease.owner == owner
                    && lease.expires_at == lease_expires_at
                    && lease.expires_at > Utc::now()
            });
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
        runtime_job_leases::delete_runtime_job_lease_receipts_tx(
            &mut tx,
            runtime_job_id,
            job.lease_generation,
        )
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
        .bind(command_status.as_str())
        .bind(&command.id)
        .execute(&mut *tx)
        .await?;
        command.status = command_status;
        command.dispatch_owner = None;
        command.dispatch_lease_expires_at = None;

        let (workflow_exists,): (bool,) =
            sqlx::query_as("SELECT EXISTS (SELECT 1 FROM workflow_instances WHERE id = $1)")
                .bind(&command.workflow_id)
                .fetch_one(&mut *tx)
                .await?;
        if !workflow_exists {
            tx.commit().await?;
            return Ok(Some(RuntimeActivityCompletion {
                runtime_job: job,
                command: Some(command),
                workflow_event: None,
                decision: None,
            }));
        }

        let active_start_child_workflow_commands =
            if command.command.command_type == WorkflowCommandType::StartChildWorkflow {
                let command_type = enum_str(&WorkflowCommandType::StartChildWorkflow)?;
                let (count,): (i64,) = sqlx::query_as(
                    "SELECT COUNT(*) FROM workflow_commands
                     WHERE workflow_id = $1
                       AND id <> $2
                       AND command_type = $3
                       AND status IN ($4, $5, $6, $7)",
                )
                .bind(&command.workflow_id)
                .bind(&command.id)
                .bind(&command_type)
                .bind(WorkflowCommandStatus::Pending.as_str())
                .bind(WorkflowCommandStatus::Dispatching.as_str())
                .bind(WorkflowCommandStatus::Dispatched.as_str())
                .bind(WorkflowCommandStatus::Deferred.as_str())
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

        let decision_record = runtime_completion::apply_runtime_completion_decision_tx(
            &mut tx,
            &command.workflow_id,
            owner,
            &event,
        )
        .await?;

        tx.commit().await?;
        if let Some(decision) = decision_record.as_ref() {
            self.record_terminal_repo_memory_for_completion(&event, decision)
                .await;
        }
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
            return Err(RuntimeJobNotFoundError::new(runtime_job_id).into());
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

        job.renew_lease(owner, next_lease_expires_at);
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

    pub async fn defer_runtime_job_claim_if_owned(
        &self,
        runtime_job_id: &str,
        owner: &str,
        lease_expires_at: DateTime<Utc>,
        not_before: DateTime<Utc>,
    ) -> anyhow::Result<Option<RuntimeJob>> {
        let mut tx = self.pool.begin().await?;
        let row: Option<(String,)> =
            sqlx::query_as("SELECT data::text FROM runtime_jobs WHERE id = $1 FOR UPDATE")
                .bind(runtime_job_id)
                .fetch_optional(&mut *tx)
                .await?;
        let Some((data,)) = row else {
            return Err(RuntimeJobNotFoundError::new(runtime_job_id).into());
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

        job.status = RuntimeJobStatus::Pending;
        job.lease = None;
        job.not_before = Some(not_before);
        job.updated_at = Utc::now();
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
        runtime_job_leases::delete_runtime_job_lease_receipts_tx(
            &mut tx,
            runtime_job_id,
            job.lease_generation,
        )
        .await?;
        tx.commit().await?;
        Ok(Some(job))
    }

    pub async fn record_runtime_job_failure_class(
        &self,
        runtime_job_id: &str,
        failure_class: &str,
    ) -> anyhow::Result<Option<RuntimeJob>> {
        let mut tx = self.pool.begin().await?;
        let row: Option<(String,)> =
            sqlx::query_as("SELECT data::text FROM runtime_jobs WHERE id = $1 FOR UPDATE")
                .bind(runtime_job_id)
                .fetch_optional(&mut *tx)
                .await?;
        let Some((data,)) = row else {
            tx.commit().await?;
            return Ok(None);
        };
        let mut job: RuntimeJob = serde_json::from_str(&data)?;
        job.failure_class = Some(failure_class.to_string());
        job.updated_at = Utc::now();
        let updated = to_jsonb_string(&job)?;
        sqlx::query(
            "UPDATE runtime_jobs
             SET data = $1::jsonb, updated_at = $2
             WHERE id = $3",
        )
        .bind(&updated)
        .bind(job.updated_at)
        .bind(runtime_job_id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(Some(job))
    }

    pub async fn defer_ready_runtime_jobs_for_profile(
        &self,
        runtime_profile: &str,
        not_before: DateTime<Utc>,
    ) -> anyhow::Result<usize> {
        let mut tx = self.pool.begin().await?;
        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT id, data::text FROM runtime_jobs
             WHERE runtime_profile = $1
               AND status = 'pending'
               AND (not_before IS NULL OR not_before <= CURRENT_TIMESTAMP)
             ORDER BY created_at ASC
             FOR UPDATE",
        )
        .bind(runtime_profile)
        .fetch_all(&mut *tx)
        .await?;
        let now = Utc::now();
        let mut deferred = 0usize;
        for (id, data) in rows {
            let mut job: RuntimeJob = serde_json::from_str(&data)?;
            job.not_before = Some(not_before);
            job.updated_at = now;
            let updated = to_jsonb_string(&job)?;
            sqlx::query(
                "UPDATE runtime_jobs
                 SET not_before = $1, data = $2::jsonb, updated_at = $3
                 WHERE id = $4",
            )
            .bind(not_before)
            .bind(&updated)
            .bind(now)
            .bind(&id)
            .execute(&mut *tx)
            .await?;
            deferred += 1;
        }
        tx.commit().await?;
        Ok(deferred)
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

    pub async fn runtime_job_matches_running_lease(
        &self,
        expected: &RuntimeJob,
    ) -> anyhow::Result<bool> {
        let Some(expected_lease) = expected.lease.as_ref() else {
            return Ok(false);
        };
        let Some(current) = self.get_runtime_job(&expected.id).await? else {
            return Ok(false);
        };
        let Some(current_lease) = current.lease.as_ref() else {
            return Ok(false);
        };
        Ok(current.status == RuntimeJobStatus::Running
            && current.lease_generation == expected.lease_generation
            && current_lease.owner == expected_lease.owner
            && current_lease.expires_at >= expected_lease.expires_at)
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
            runtime_job_leases::delete_runtime_job_lease_receipts_tx(
                &mut tx,
                &id,
                job.lease_generation,
            )
            .await?;
            cancelled += 1;
        }
        sqlx::query(
            "UPDATE workflow_commands
             SET status = $2,
                 dispatch_owner = NULL,
                 dispatch_lease_expires_at = NULL,
                 dispatch_not_before = NULL,
                 dispatch_barrier = NULL,
                 updated_at = CURRENT_TIMESTAMP
             WHERE id = $1
               AND status IN ($3, $4, $5, $6)",
        )
        .bind(command_id)
        .bind(WorkflowCommandStatus::Cancelled.as_str())
        .bind(WorkflowCommandStatus::Pending.as_str())
        .bind(WorkflowCommandStatus::Dispatching.as_str())
        .bind(WorkflowCommandStatus::Dispatched.as_str())
        .bind(WorkflowCommandStatus::Deferred.as_str())
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

    pub async fn command_sources_for_runtime_jobs(
        &self,
        runtime_job_ids: &[String],
    ) -> anyhow::Result<BTreeMap<String, RuntimeJobCommandSource>> {
        if runtime_job_ids.is_empty() {
            return Ok(BTreeMap::new());
        }
        let rows: Vec<(String, String, String, String)> = sqlx::query_as(
            "SELECT job.id, command.workflow_id, command.id, command.data::text
             FROM runtime_jobs AS job
             JOIN workflow_commands AS command ON command.id = job.command_id
             WHERE job.id = ANY($1::text[])",
        )
        .bind(runtime_job_ids)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|(runtime_job_id, workflow_id, command_id, data)| {
                let command = serde_json::from_str(&data)?;
                Ok((
                    runtime_job_id.clone(),
                    RuntimeJobCommandSource {
                        runtime_job_id,
                        workflow_id,
                        command_id,
                        command,
                    },
                ))
            })
            .collect()
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
                 SELECT command_id, data::text AS data, created_at, (data->>'created_at')::timestamptz AS job_created_at
                 FROM runtime_jobs
                 WHERE command_id = selected.command_id
                 ORDER BY created_at DESC, (data->>'created_at')::timestamptz DESC
                 LIMIT $2
             ) AS job ON true
             ORDER BY job.command_id ASC, job.created_at ASC, job.job_created_at ASC",
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

    pub async fn compact_runtime_jobs_for_commands_limited(
        &self,
        command_ids: &[String],
        per_command_limit: i64,
    ) -> anyhow::Result<BTreeMap<String, Vec<RuntimeJobCompactRecord>>> {
        if command_ids.is_empty() || per_command_limit <= 0 {
            return Ok(BTreeMap::new());
        }
        let per_command_limit = per_command_limit.clamp(1, 50);
        let rows: Vec<RuntimeJobCompactRecordRow> = sqlx::query_as(
            "SELECT job.command_id, job.id, job.runtime_kind, job.runtime_profile,
                    job.status, (job.data->'lease')::text AS lease,
                    COALESCE((job.data->>'lease_generation')::bigint, 0),
                    job.data->>'error',
                    job.data->>'failure_class',
                    (job.data->>'not_before')::timestamptz,
                    job.created_at, job.updated_at
             FROM unnest($1::text[]) AS selected(command_id)
             JOIN LATERAL (
                 SELECT command_id, id, runtime_kind, runtime_profile, status, data,
                        created_at, updated_at,
                        (data->>'created_at')::timestamptz AS job_created_at
                 FROM runtime_jobs
                 WHERE command_id = selected.command_id
                 ORDER BY created_at DESC, (data->>'created_at')::timestamptz DESC
                 LIMIT $2
             ) AS job ON true
             ORDER BY job.command_id ASC, job.created_at ASC, job.job_created_at ASC",
        )
        .bind(command_ids)
        .bind(per_command_limit)
        .fetch_all(&self.pool)
        .await?;
        let mut by_command = BTreeMap::new();
        for (
            command_id,
            id,
            runtime_kind,
            runtime_profile,
            status,
            lease_json,
            lease_generation,
            error,
            failure_class,
            not_before,
            created_at,
            updated_at,
        ) in rows
        {
            let lease = lease_json
                .as_deref()
                .filter(|value| *value != "null")
                .map(serde_json::from_str)
                .transpose()?;
            let record = RuntimeJobCompactRecord {
                id,
                command_id: command_id.clone(),
                runtime_kind: enum_from_str(&runtime_kind)?,
                runtime_profile,
                status: enum_from_str(&status)?,
                lease,
                lease_generation: lease_generation.max(0) as u64,
                error,
                failure_class,
                not_before,
                created_at,
                updated_at,
            };
            by_command
                .entry(command_id)
                .or_insert_with(Vec::new)
                .push(record);
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

fn enum_from_str<T>(value: &str) -> anyhow::Result<T>
where
    T: DeserializeOwned,
{
    Ok(serde_json::from_value(Value::String(value.to_string()))?)
}

async fn runtime_job_for_command_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    command_id: &str,
) -> anyhow::Result<Option<RuntimeJob>> {
    let row: Option<(String,)> = sqlx::query_as(
        "SELECT data::text FROM runtime_jobs
         WHERE command_id = $1
         ORDER BY created_at DESC, (data->>'created_at')::timestamptz DESC
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

pub(super) async fn insert_event_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workflow_id: &str,
    event_type: &str,
    source: &str,
    payload: Value,
) -> anyhow::Result<WorkflowEvent> {
    insert_event_tx_with_id(tx, workflow_id, event_type, source, payload, None).await
}

async fn insert_event_tx_with_id(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    workflow_id: &str,
    event_type: &str,
    source: &str,
    payload: Value,
    event_id: Option<&str>,
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
    let mut event = WorkflowEvent::new(workflow_id, next_sequence as u64, event_type, source)
        .with_payload(payload);
    if let Some(event_id) = event_id {
        event.id = event_id.to_string();
    }
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
        WorkflowCommandType::MarkFailed
        | WorkflowCommandType::MarkBlocked
        | WorkflowCommandType::MarkCancelled => {
            super::worker::apply_failure_reason_side_effect(instance, command)
        }
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

fn command_status_for_activity(status: ActivityStatus) -> WorkflowCommandStatus {
    match status {
        ActivityStatus::Succeeded => WorkflowCommandStatus::Completed,
        ActivityStatus::Failed => WorkflowCommandStatus::Failed,
        ActivityStatus::Blocked => WorkflowCommandStatus::Blocked,
        ActivityStatus::Cancelled => WorkflowCommandStatus::Cancelled,
    }
}

fn validator_for_definition(definition_id: &str) -> Option<DecisionValidator> {
    super::state_registry::decision_validator_for_definition(definition_id)
}
