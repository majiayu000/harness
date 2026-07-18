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
use super::transcript::PendingRuntimeTranscript;
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
#[path = "store/artifacts.rs"]
mod artifacts;
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
#[path = "store/runtime_job_state.rs"]
mod runtime_job_state;
#[path = "store/runtime_jobs.rs"]
mod runtime_jobs;
#[path = "store/runtime_usage.rs"]
mod runtime_usage;
#[path = "store/submission_commit.rs"]
mod submission_commit;
#[path = "store/submission_instances.rs"]
mod submission_instances;
#[path = "store/terminal_instance_queries.rs"]
mod terminal_instance_queries;
#[path = "store/transaction_helpers.rs"]
mod transaction_helpers;
pub use driverless_progress::{DriverlessProgressInstance, DriverlessProgressProvenanceStatus};
pub use recovery::{
    WorkflowRuntimeRecoveryAction, WorkflowRuntimeRecoveryOutcome, WorkflowRuntimeRecoveryRequest,
};
pub use runtime_usage::{
    cost_usd_from_micros, cost_usd_to_micros, RuntimeUsageMetrics, RuntimeUsageRecord,
    RuntimeUsageUpsert, RuntimeUsageUpsertOutcome, RuntimeWorkflowUsage,
};
pub use submission_commit::{
    WorkflowSubmissionDecisionCommit, WorkflowSubmissionDecisionTransition,
    WorkflowSubmissionPromptPayload,
};
pub use submission_instances::WorkflowSubmissionFilter;
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
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct WorkflowSubmissionMetrics {
    pub global_done: u64,
    pub global_failed: u64,
    pub global_stalled: u64,
    pub latest_pr_url: Option<String>,
    pub done_since: u64,
    pub by_project: BTreeMap<String, WorkflowSubmissionProjectMetrics>,
    pub hourly_done: Vec<WorkflowSubmissionHourlyDone>,
}
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct WorkflowSubmissionProjectMetrics {
    pub done: u64,
    pub failed: u64,
    pub stalled: u64,
    pub latest_pr_url: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowSubmissionHourlyDone {
    pub project_id: String,
    pub hour: String,
    pub count: u64,
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
}
