use super::attestation::EvalAttestationSummary;
#[cfg(test)]
use super::evidence_usage::usage_snapshot_from_event;
use super::evidence_usage::usage_snapshots;
#[cfg(test)]
use super::model::Confidence;
use super::model::{
    QualitySnapshot, RuntimeArtifactSnapshot, RuntimeErrorKind, RuntimeJobSnapshot,
    RuntimeSnapshot, UsageSnapshot,
};
use super::verification_evidence::{validation_command_evidence, EvalValidationCommandEvidence};
use crate::runtime::{
    ActivityErrorKind, ActivityResult, RuntimeEvent, RuntimeJob, RuntimeJobStatus,
    WorkflowCommandRecord, WorkflowInstance, QUALITY_GATE_ACTIVITY,
};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvalEvidenceStatus {
    Passed,
    Failed,
    Skipped,
    TimedOut,
    DispatchFailed,
    EvidenceIncomplete,
    BudgetExhausted,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EvalCaseEvidence {
    pub eval_run_id: String,
    pub case_id: String,
    pub workflow_id: Option<String>,
    pub status: EvalEvidenceStatus,
    #[serde(default)]
    pub attestation: EvalAttestationSummary,
    pub runtime: Option<RuntimeSnapshot>,
    pub usage: Vec<UsageSnapshot>,
    pub submission: Option<EvalSubmissionEvidence>,
    pub quality_gate: Option<EvalQualityGateEvidence>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quality: Option<QualitySnapshot>,
    #[serde(default)]
    pub isolation: Option<EvalIsolationEvidence>,
    pub missing_evidence: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvalSubmissionEvidence {
    pub repo: Option<String>,
    pub issue_number: Option<u64>,
    pub command_id: Option<String>,
    pub command_status: Option<String>,
    pub runtime_job_ids: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvalQualityGateEvidence {
    pub command_id: Option<String>,
    pub runtime_job_id: Option<String>,
    pub status: String,
    pub validation_passed: bool,
    pub validation_commands: Vec<String>,
    #[serde(default)]
    pub validation_evidence: Vec<EvalValidationCommandEvidence>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvalIsolationEvidence {
    pub required_tier: Option<String>,
    pub selected_tier: Option<String>,
    pub runtime_kind: Option<String>,
    pub runtime_profile: Option<String>,
    pub sandbox: Option<String>,
    pub backend: Option<String>,
    pub image: Option<String>,
    pub lifecycle: Option<String>,
    pub cleanup_required: bool,
    pub cleanup_status: Option<String>,
}

pub fn collect_eval_case_evidence_from_records(
    eval_run_id: &str,
    case_id: &str,
    workflow: Option<&WorkflowInstance>,
    commands: &[WorkflowCommandRecord],
    runtime_jobs: &[RuntimeJob],
    runtime_events: &BTreeMap<String, Vec<RuntimeEvent>>,
) -> EvalCaseEvidence {
    let mut missing_evidence = Vec::new();
    let workflow_id = workflow.map(|workflow| workflow.id.clone());
    if workflow.is_none() {
        missing_evidence.push("workflow_instance".to_string());
    }

    let submission = submission_evidence(workflow, commands, runtime_jobs);
    if submission.is_none() {
        missing_evidence.push("submission".to_string());
    } else if submission
        .as_ref()
        .is_some_and(|submission| submission.runtime_job_ids.is_empty())
    {
        missing_evidence.push("submission_runtime_job".to_string());
    }
    let quality_gate = quality_gate_evidence(commands, runtime_jobs);
    if quality_gate.is_none() {
        missing_evidence.push("quality_gate".to_string());
    } else if quality_gate
        .as_ref()
        .is_some_and(|quality_gate| !quality_gate.validation_passed)
    {
        missing_evidence.push("quality_gate_pass".to_string());
    }
    let isolation = isolation_evidence(workflow, commands, runtime_jobs);
    match isolation.as_ref() {
        Some(isolation) => missing_evidence.extend(validate_isolation_evidence(isolation)),
        None => missing_evidence.push("isolation_policy".to_string()),
    }

    let runtime = workflow.map(|workflow| runtime_snapshot(workflow, runtime_jobs));
    if runtime.as_ref().is_none_or(|snapshot| {
        snapshot.terminal_state.is_none()
            && snapshot
                .runtime_jobs
                .iter()
                .all(|job| job.terminal_state.is_none())
    }) {
        missing_evidence.push("terminal_runtime_state".to_string());
    }
    let usage = usage_snapshots(workflow_id.as_deref(), runtime_events, runtime_jobs);
    if usage.is_empty() {
        missing_evidence.push("usage".to_string());
    }

    let status = if missing_evidence.is_empty() {
        EvalEvidenceStatus::Passed
    } else {
        EvalEvidenceStatus::Failed
    };

    EvalCaseEvidence {
        eval_run_id: eval_run_id.to_string(),
        case_id: case_id.to_string(),
        workflow_id,
        status,
        attestation: EvalAttestationSummary::unsigned(),
        runtime,
        usage,
        submission,
        quality_gate,
        quality: None,
        isolation,
        missing_evidence,
    }
}

fn submission_evidence(
    workflow: Option<&WorkflowInstance>,
    commands: &[WorkflowCommandRecord],
    runtime_jobs: &[RuntimeJob],
) -> Option<EvalSubmissionEvidence> {
    let workflow = workflow?;
    let implementation_command = commands
        .iter()
        .find(|command| command.command.runtime_activity_key() == "implement_issue")?;
    let command_id = Some(implementation_command.id.clone());
    let command_status = Some(implementation_command.status.as_str().to_string());
    let runtime_job_ids = command_id
        .as_deref()
        .map(|command_id| {
            runtime_jobs
                .iter()
                .filter(|job| job.command_id == command_id)
                .map(|job| job.id.clone())
                .collect()
        })
        .unwrap_or_default();
    Some(EvalSubmissionEvidence {
        repo: workflow
            .data
            .get("repo")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        issue_number: workflow.data.get("issue_number").and_then(Value::as_u64),
        command_id,
        command_status,
        runtime_job_ids,
    })
}

fn quality_gate_evidence(
    commands: &[WorkflowCommandRecord],
    runtime_jobs: &[RuntimeJob],
) -> Option<EvalQualityGateEvidence> {
    let command = commands
        .iter()
        .find(|command| command.command.runtime_activity_key() == QUALITY_GATE_ACTIVITY);
    let command_id = command.map(|command| command.id.clone());
    let job = command_id
        .as_deref()
        .and_then(|command_id| runtime_jobs.iter().find(|job| job.command_id == command_id))
        .or_else(|| {
            runtime_jobs.iter().find(|job| {
                job.input
                    .get("activity")
                    .and_then(Value::as_str)
                    .is_some_and(|activity| activity == QUALITY_GATE_ACTIVITY)
                    || activity_result_from_job(job)
                        .as_ref()
                        .is_some_and(|result| result.activity == QUALITY_GATE_ACTIVITY)
            })
        })?;
    let result = activity_result_from_job(job);
    let validation_commands = result
        .as_ref()
        .map(|result| {
            result
                .validation
                .iter()
                .map(|record| record.command.clone())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let validation_passed = result.as_ref().is_some_and(|result| {
        result.status == crate::runtime::ActivityStatus::Succeeded
            && !result.validation.is_empty()
            && result
                .validation
                .iter()
                .all(|record| record.status.eq_ignore_ascii_case("passed"))
    });
    Some(EvalQualityGateEvidence {
        command_id,
        runtime_job_id: Some(job.id.clone()),
        status: eval_runtime_job_status(job.status).to_string(),
        validation_passed,
        validation_commands,
        validation_evidence: validation_command_evidence(result.as_ref()),
    })
}

fn isolation_evidence(
    workflow: Option<&WorkflowInstance>,
    commands: &[WorkflowCommandRecord],
    runtime_jobs: &[RuntimeJob],
) -> Option<EvalIsolationEvidence> {
    let required = required_eval_isolation(workflow, commands)?;
    let implementation_job = implementation_runtime_job(commands, runtime_jobs);
    Some(EvalIsolationEvidence {
        required_tier: string_field(required, "tier"),
        selected_tier: implementation_job
            .and_then(|job| job.input.pointer("/isolation/tier"))
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        runtime_kind: implementation_job.map(|job| job.runtime_kind.as_str().to_string()),
        runtime_profile: implementation_job.map(|job| job.runtime_profile.clone()),
        sandbox: implementation_job
            .and_then(|job| job.input.pointer("/runtime_profile/sandbox"))
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
            .or_else(|| string_field(required, "sandbox")),
        backend: string_field(required, "backend"),
        image: string_field(required, "image"),
        lifecycle: string_field(required, "lifecycle"),
        cleanup_required: required
            .get("cleanup_required")
            .and_then(Value::as_bool)
            .unwrap_or(true),
        cleanup_status: implementation_job
            .and_then(activity_result_from_job)
            .and_then(|result| {
                result.artifacts.into_iter().find(|artifact| {
                    artifact.artifact_type
                        == crate::runtime::completion_evidence::ARTIFACT_EVAL_ISOLATION_CLEANUP
                })
            })
            .and_then(|artifact| {
                artifact
                    .artifact
                    .get("status")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned)
            })
            .or_else(|| {
                workflow
                    .and_then(|workflow| workflow.data.pointer("/eval/cleanup/status"))
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned)
            }),
    })
}

fn required_eval_isolation<'a>(
    workflow: Option<&'a WorkflowInstance>,
    commands: &'a [WorkflowCommandRecord],
) -> Option<&'a Value> {
    commands
        .iter()
        .find_map(|command| command.command.command.pointer("/eval/isolation"))
        .or_else(|| workflow.and_then(|workflow| workflow.data.pointer("/eval/isolation")))
}

fn implementation_runtime_job<'a>(
    commands: &[WorkflowCommandRecord],
    runtime_jobs: &'a [RuntimeJob],
) -> Option<&'a RuntimeJob> {
    let implementation_command_id = commands
        .iter()
        .find(|command| command.command.runtime_activity_key() == "implement_issue")
        .map(|command| command.id.as_str());
    implementation_command_id
        .and_then(|command_id| runtime_jobs.iter().find(|job| job.command_id == command_id))
        .or_else(|| {
            runtime_jobs.iter().find(|job| {
                job.input
                    .get("activity")
                    .and_then(Value::as_str)
                    .is_some_and(|activity| activity == "implement_issue")
                    || activity_result_from_job(job)
                        .as_ref()
                        .is_some_and(|result| result.activity == "implement_issue")
            })
        })
}

fn validate_isolation_evidence(isolation: &EvalIsolationEvidence) -> Vec<String> {
    let mut missing = Vec::new();
    if isolation.required_tier.as_deref() != Some("container") {
        missing.push("isolation_required_tier".to_string());
    }
    if isolation.selected_tier.as_deref() != isolation.required_tier.as_deref() {
        missing.push("isolation_selected_tier".to_string());
    }
    if isolation.runtime_kind.as_deref() != Some("remote_host") {
        missing.push("runtime_host".to_string());
    }
    if isolation
        .runtime_profile
        .as_deref()
        .is_none_or(str::is_empty)
    {
        missing.push("runtime_profile".to_string());
    }
    if isolation.sandbox.as_deref() != Some("workspace-write") {
        missing.push("isolation_sandbox".to_string());
    }
    if isolation.backend.as_deref().is_none_or(str::is_empty) {
        missing.push("isolation_backend".to_string());
    }
    if isolation.image.as_deref().is_none_or(str::is_empty) {
        missing.push("isolation_image".to_string());
    }
    if isolation.lifecycle.as_deref() != Some("ephemeral") {
        missing.push("isolation_lifecycle".to_string());
    }
    if isolation.cleanup_required && isolation.cleanup_status.as_deref() != Some("cleaned") {
        missing.push("isolation_cleanup".to_string());
    }
    missing
}

fn string_field(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
}

fn runtime_snapshot(workflow: &WorkflowInstance, runtime_jobs: &[RuntimeJob]) -> RuntimeSnapshot {
    RuntimeSnapshot {
        task_id: workflow
            .data
            .get("task_id")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        workflow_id: Some(workflow.id.clone()),
        workflow_state: Some(workflow.state.clone()),
        runtime_jobs: runtime_jobs.iter().map(runtime_job_snapshot).collect(),
        latest_activity: runtime_jobs
            .iter()
            .rev()
            .find_map(|job| job.input.get("activity").and_then(Value::as_str))
            .map(ToOwned::to_owned),
        terminal_state: workflow.is_terminal().then(|| workflow.state.clone()),
        collected_at: Utc::now().to_rfc3339(),
    }
}

fn runtime_job_snapshot(job: &RuntimeJob) -> RuntimeJobSnapshot {
    let result = activity_result_from_job(job);
    RuntimeJobSnapshot {
        runtime_job_id: job.id.clone(),
        state: eval_runtime_job_status(job.status).to_string(),
        activity: job
            .input
            .get("activity")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
            .or_else(|| result.as_ref().map(|result| result.activity.clone())),
        artifact_count: result
            .as_ref()
            .map(|result| result.artifacts.len() as u64)
            .unwrap_or(0),
        artifacts: result
            .as_ref()
            .map(|result| {
                result
                    .artifacts
                    .iter()
                    .map(|artifact| RuntimeArtifactSnapshot {
                        artifact_type: artifact.artifact_type.clone(),
                        artifact: artifact.artifact.clone(),
                    })
                    .collect()
            })
            .unwrap_or_default(),
        terminal_state: runtime_job_terminal_state(job).map(ToOwned::to_owned),
        error_kind: result
            .as_ref()
            .and_then(|result| result.error_kind.map(runtime_error_kind)),
    }
}

pub(super) fn activity_result_from_job(job: &RuntimeJob) -> Option<ActivityResult> {
    let output = job.output.as_ref()?;
    match serde_json::from_value(output.clone()) {
        Ok(result) => Some(result),
        Err(error) => {
            // A malformed payload must not be silently treated as absent:
            // downstream evidence would look identical to a legitimately
            // skipped step, hiding producer bugs from operators.
            let tagged_activity = job.input.get("activity").and_then(Value::as_str);
            tracing::warn!(
                job_id = %job.id,
                activity = tagged_activity.unwrap_or("untagged"),
                %error,
                "eval evidence: runtime job output failed to deserialize into ActivityResult"
            );
            None
        }
    }
}

fn runtime_job_terminal_state(job: &RuntimeJob) -> Option<&'static str> {
    match job.status {
        RuntimeJobStatus::Succeeded => Some("succeeded"),
        RuntimeJobStatus::Failed => Some("failed"),
        RuntimeJobStatus::Cancelled => Some("cancelled"),
        RuntimeJobStatus::Pending | RuntimeJobStatus::Running => None,
    }
}

fn eval_runtime_job_status(status: RuntimeJobStatus) -> &'static str {
    match status {
        RuntimeJobStatus::Pending => "pending",
        RuntimeJobStatus::Running => "running",
        RuntimeJobStatus::Succeeded => "succeeded",
        RuntimeJobStatus::Failed => "failed",
        RuntimeJobStatus::Cancelled => "cancelled",
    }
}

fn runtime_error_kind(kind: ActivityErrorKind) -> RuntimeErrorKind {
    match kind {
        ActivityErrorKind::Retryable => RuntimeErrorKind::Retryable,
        ActivityErrorKind::Timeout => RuntimeErrorKind::Timeout,
        ActivityErrorKind::Fatal | ActivityErrorKind::SpawnFailure => RuntimeErrorKind::Fatal,
        ActivityErrorKind::Configuration => RuntimeErrorKind::Configuration,
        ActivityErrorKind::ExternalDependency => RuntimeErrorKind::ExternalDependency,
        ActivityErrorKind::Unknown => RuntimeErrorKind::Unknown,
    }
}

#[cfg(test)]
#[path = "evidence_tests.rs"]
mod tests;
