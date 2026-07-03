use serde_json::Value;

use crate::runtime_projection::{RuntimeActiveBucket, RuntimeWorkflowProjection};
use crate::task_runner;
use harness_workflow::runtime::store::{RuntimeEventSummary, RuntimeJobCompactRecord};
use harness_workflow::runtime::{
    runtime_job_running_lease_state_at, RuntimeEvent, RuntimeJob, RuntimeJobStatus,
    WorkflowCommand, WorkflowCommandRecord, WorkflowDecisionRecord, WorkflowEvent,
    WorkflowInstance, WorkflowLease,
};

pub(super) const ACTIVITY_RESULT_ENVELOPE_ARTIFACT_TYPE: &str = "activity_result_envelope";
pub(super) const ACTIVITY_RESULT_ENVELOPE_SCHEMA: &str =
    "harness.runtime.activity_result_envelope.v1";

#[derive(Debug, Clone, serde::Serialize)]
pub(super) struct WorkflowRuntimeCommandNode {
    pub id: String,
    pub workflow_id: String,
    pub decision_id: Option<String>,
    pub status: String,
    pub command: WorkflowCommand,
    pub runtime_job_count: usize,
    pub runtime_jobs: Vec<WorkflowRuntimeJobNode>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

impl WorkflowRuntimeCommandNode {
    pub(super) fn new(
        record: WorkflowCommandRecord,
        runtime_job_count: usize,
        runtime_jobs: Vec<WorkflowRuntimeJobNode>,
    ) -> Self {
        Self {
            id: record.id,
            workflow_id: record.workflow_id,
            decision_id: record.decision_id,
            status: record.status.to_string(),
            command: record.command,
            runtime_job_count,
            runtime_jobs,
            created_at: record.created_at,
            updated_at: record.updated_at,
        }
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub(super) struct WorkflowRuntimeJobNode {
    pub id: String,
    pub command_id: String,
    pub runtime_kind: harness_workflow::runtime::RuntimeKind,
    pub runtime_profile: String,
    pub status: RuntimeJobStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lease: Option<WorkflowLease>,
    pub lease_generation: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure_class: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub not_before: Option<chrono::DateTime<chrono::Utc>>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
    pub runtime_event_count: usize,
    pub latest_runtime_event_type: Option<String>,
    pub prompt_packet_digest: Option<String>,
    pub activity_result_envelope: Option<Value>,
    pub lease_state: Option<&'static str>,
    pub in_flight_model_turn: bool,
    pub last_runtime_observation_at: Option<chrono::DateTime<chrono::Utc>>,
}

impl WorkflowRuntimeJobNode {
    pub(super) fn full(job: RuntimeJob, runtime_events: Vec<RuntimeEvent>) -> Self {
        let latest_runtime_event_type = runtime_events.last().map(|event| event.event_type.clone());
        let prompt_packet_digest = prompt_packet_digest_from_events(&runtime_events);
        let activity_result_envelope = activity_result_envelope_from_job(&job);
        let lease_state = runtime_job_running_lease_state_at(&job, chrono::Utc::now())
            .map(|state| state.status_label());
        let in_flight_model_turn = runtime_job_has_in_flight_model_turn(&job, &runtime_events);
        let last_runtime_observation_at = last_runtime_observation_at(&job, &runtime_events);
        Self {
            id: job.id,
            command_id: job.command_id,
            runtime_kind: job.runtime_kind,
            runtime_profile: job.runtime_profile,
            status: job.status,
            lease: job.lease,
            lease_generation: job.lease_generation,
            input: Some(job.input),
            output: job.output,
            error: job.error,
            failure_class: job.failure_class,
            not_before: job.not_before,
            created_at: job.created_at,
            updated_at: job.updated_at,
            runtime_event_count: runtime_events.len(),
            latest_runtime_event_type,
            prompt_packet_digest,
            activity_result_envelope,
            lease_state,
            in_flight_model_turn,
            last_runtime_observation_at,
        }
    }

    pub(super) fn compact(
        job: RuntimeJobCompactRecord,
        runtime_summary: RuntimeEventSummary,
    ) -> Self {
        let now = chrono::Utc::now();
        let lease_state = if job.status == RuntimeJobStatus::Running {
            Some(match job.lease.as_ref() {
                Some(lease) if lease.expires_at > now => "active_leased",
                Some(_) => "expired_lease",
                None => "missing_lease",
            })
        } else {
            None
        };
        let latest_turn_sequence = runtime_summary.latest_turn_sequence.unwrap_or(0);
        let latest_activity_result_sequence =
            runtime_summary.latest_activity_result_sequence.unwrap_or(0);
        let in_flight_model_turn = job.status == RuntimeJobStatus::Running
            && latest_turn_sequence > latest_activity_result_sequence;
        let last_runtime_observation_at = if job.status == RuntimeJobStatus::Running {
            Some(
                runtime_summary
                    .latest_runtime_event_at
                    .map(|created_at| created_at.max(job.updated_at))
                    .unwrap_or(job.updated_at),
            )
        } else {
            runtime_summary.latest_runtime_event_at
        };
        Self {
            id: job.id,
            command_id: job.command_id,
            runtime_kind: job.runtime_kind,
            runtime_profile: job.runtime_profile,
            status: job.status,
            lease: job.lease,
            lease_generation: job.lease_generation,
            input: None,
            output: None,
            error: job.error,
            failure_class: job.failure_class,
            not_before: job.not_before,
            created_at: job.created_at,
            updated_at: job.updated_at,
            runtime_event_count: runtime_summary.runtime_event_count,
            latest_runtime_event_type: runtime_summary.latest_runtime_event_type,
            prompt_packet_digest: runtime_summary.prompt_packet_digest,
            activity_result_envelope: None,
            lease_state,
            in_flight_model_turn,
            last_runtime_observation_at,
        }
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub(super) struct WorkflowRuntimeTreeNode {
    pub workflow: WorkflowInstance,
    pub projection: WorkflowRuntimeTreeProjection,
    pub runtime_job_count: usize,
    pub event_count: usize,
    pub decision_count: usize,
    pub rejected_decision_count: usize,
    pub command_count: usize,
    pub events: Vec<WorkflowEvent>,
    pub decisions: Vec<WorkflowDecisionRecord>,
    pub commands: Vec<WorkflowRuntimeCommandNode>,
    pub children: Vec<WorkflowRuntimeTreeNode>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub(super) struct WorkflowRuntimeTreeProjection {
    pub status: task_runner::TaskStatus,
    pub failure_kind: Option<task_runner::TaskFailureKind>,
    pub phase: task_runner::TaskPhase,
    pub scheduler: task_runner::TaskSchedulerState,
    pub project_id: Option<String>,
    pub submission_handle: Option<String>,
    pub legacy_dedupe_task_handle: Option<String>,
    pub active_bucket: Option<&'static str>,
}

impl WorkflowRuntimeTreeProjection {
    pub(super) fn from_workflow(workflow: &WorkflowInstance) -> Self {
        let projection = RuntimeWorkflowProjection::from_workflow(workflow);
        let active_bucket = projection.active_bucket().map(runtime_active_bucket_label);
        Self {
            status: projection.task_status,
            failure_kind: projection.failure_kind,
            phase: projection.phase,
            scheduler: projection.scheduler,
            project_id: projection.project_id,
            submission_handle: projection.submission_handle.map(|task_id| task_id.0),
            legacy_dedupe_task_handle: projection
                .legacy_dedupe_task_handle
                .map(|task_id| task_id.0),
            active_bucket,
        }
    }
}

fn runtime_active_bucket_label(bucket: RuntimeActiveBucket) -> &'static str {
    match bucket {
        RuntimeActiveBucket::Running => "running",
        RuntimeActiveBucket::Queued => "queued",
    }
}

pub(super) fn compact_workflow_command(command: WorkflowCommand) -> WorkflowCommand {
    let WorkflowCommand {
        command_type,
        dedupe_key,
        command,
    } = command;
    let Value::Object(object) = command else {
        return WorkflowCommand {
            command_type,
            dedupe_key,
            command,
        };
    };

    let mut payload = serde_json::Map::new();
    for key in [
        "activity",
        "definition_id",
        "subject_key",
        "pr_number",
        "pr_url",
        "reason",
        "concern",
    ] {
        if let Some(value) = object.get(key) {
            payload.insert(key.to_string(), value.clone());
        }
    }
    WorkflowCommand {
        command_type,
        dedupe_key,
        command: Value::Object(payload),
    }
}

fn prompt_packet_digest_from_events(events: &[RuntimeEvent]) -> Option<String> {
    events.iter().rev().find_map(|event| {
        if event.event_type != "RuntimePromptPrepared" {
            return None;
        }
        event
            .event
            .get("prompt_packet_digest")
            .and_then(Value::as_str)
            .map(str::to_string)
    })
}

pub(super) fn runtime_job_has_in_flight_model_turn(
    job: &RuntimeJob,
    events: &[RuntimeEvent],
) -> bool {
    if job.status != RuntimeJobStatus::Running {
        return false;
    }
    let Some(latest_turn_sequence) = events
        .iter()
        .filter(|event| event.event_type == "RuntimeTurnStarted")
        .map(|event| event.sequence)
        .max()
    else {
        return false;
    };
    !events.iter().any(|event| {
        event.event_type == "ActivityResultReady" && event.sequence > latest_turn_sequence
    })
}

fn last_runtime_observation_at(
    job: &RuntimeJob,
    events: &[RuntimeEvent],
) -> Option<chrono::DateTime<chrono::Utc>> {
    let latest_event_at = events.last().map(|event| event.created_at);
    if job.status == RuntimeJobStatus::Running {
        return Some(match latest_event_at {
            Some(created_at) => created_at.max(job.updated_at),
            None => job.updated_at,
        });
    }
    latest_event_at
}

pub(super) fn activity_result_envelope_from_job(job: &RuntimeJob) -> Option<Value> {
    let artifacts = job.output.as_ref()?.get("artifacts")?.as_array()?;
    artifacts.iter().rev().find_map(|artifact| {
        if artifact.get("artifact_type").and_then(Value::as_str)
            != Some(ACTIVITY_RESULT_ENVELOPE_ARTIFACT_TYPE)
        {
            return None;
        }
        let payload = artifact.get("artifact")?;
        if payload.get("schema").and_then(Value::as_str) != Some(ACTIVITY_RESULT_ENVELOPE_SCHEMA) {
            return None;
        }
        Some(payload.clone())
    })
}
