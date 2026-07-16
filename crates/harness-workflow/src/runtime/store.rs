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
use anyhow::Context;
use chrono::{DateTime, Utc};
use harness_core::db::PgStoreContext;
use serde::{de::DeserializeOwned, Serialize};
use serde_json::{json, Value};
use sqlx::postgres::PgPool;
use std::collections::BTreeMap;
use std::path::Path;

#[path = "store/activity_completion.rs"]
mod activity_completion;
#[path = "store/command_facade.rs"]
mod command_facade;
#[path = "store/commands.rs"]
mod command_store;
#[path = "store/decisions.rs"]
mod decisions;
#[path = "store/definitions.rs"]
mod definitions;
#[path = "store/driverless_progress.rs"]
mod driverless_progress;
#[path = "store/events.rs"]
mod events;
#[path = "store/instance_helpers.rs"]
mod instance_helpers;
#[path = "store/instances.rs"]
mod instances;
#[path = "store/prompt_payloads.rs"]
mod prompt_payloads;
#[path = "store/recovery.rs"]
mod recovery;
#[path = "store/runtime_completion.rs"]
mod runtime_completion;
#[path = "store/runtime_job_leases.rs"]
pub mod runtime_job_leases;
#[path = "store/runtime_job_queries.rs"]
mod runtime_job_queries;
#[path = "store/runtime_jobs.rs"]
mod runtime_jobs;
#[path = "store/runtime_usage.rs"]
mod runtime_usage;
#[path = "store/submission_commit.rs"]
mod submission_commit;
#[path = "store/transaction_helpers.rs"]
mod transaction_helpers;
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
use transaction_helpers::{
    apply_inline_command_side_effect, insert_decision_record_tx, insert_event_tx_with_id,
    insert_instance_if_absent_tx, load_or_insert_initial_instance_tx, runtime_job_for_command_tx,
    select_instance_for_update_tx, upsert_instance_tx,
};
pub(super) use transaction_helpers::{enum_str, insert_event_tx, to_jsonb_string};
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
}
