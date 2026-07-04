use super::model::{
    Confidence, RuntimeErrorKind, RuntimeJobSnapshot, RuntimeSnapshot, UsageSnapshot,
};
use crate::runtime::{
    ActivityErrorKind, ActivityResult, RuntimeEvent, RuntimeJob, RuntimeJobStatus,
    WorkflowCommandRecord, WorkflowInstance, WorkflowRuntimeStore, QUALITY_GATE_ACTIVITY,
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
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EvalCaseEvidence {
    pub eval_run_id: String,
    pub case_id: String,
    pub workflow_id: Option<String>,
    pub status: EvalEvidenceStatus,
    pub runtime: Option<RuntimeSnapshot>,
    pub usage: Vec<UsageSnapshot>,
    pub submission: Option<EvalSubmissionEvidence>,
    pub quality_gate: Option<EvalQualityGateEvidence>,
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
}

pub async fn collect_eval_case_evidence(
    store: &WorkflowRuntimeStore,
    eval_run_id: &str,
    case_id: &str,
    workflow_id: &str,
) -> anyhow::Result<EvalCaseEvidence> {
    let workflow = store.get_instance(workflow_id).await?;
    let commands = store.commands_for(workflow_id).await?;
    let command_ids = commands
        .iter()
        .map(|command| command.id.clone())
        .collect::<Vec<_>>();
    let jobs_by_command = store.runtime_jobs_for_commands(&command_ids).await?;
    let runtime_jobs = command_ids
        .iter()
        .filter_map(|command_id| jobs_by_command.get(command_id))
        .flatten()
        .cloned()
        .collect::<Vec<_>>();
    let runtime_job_ids = runtime_jobs
        .iter()
        .map(|job| job.id.clone())
        .collect::<Vec<_>>();
    let events_by_job = store.runtime_events_for_jobs(&runtime_job_ids).await?;

    Ok(collect_eval_case_evidence_from_records(
        eval_run_id,
        case_id,
        workflow.as_ref(),
        &commands,
        &runtime_jobs,
        &events_by_job,
    ))
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
    let usage = usage_snapshots(workflow_id.as_deref(), runtime_events);
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
        runtime,
        usage,
        submission,
        quality_gate,
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
    })
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
        terminal_state: runtime_job_terminal_state(job),
        error_kind: result
            .as_ref()
            .and_then(|result| result.error_kind.map(runtime_error_kind)),
    }
}

fn activity_result_from_job(job: &RuntimeJob) -> Option<ActivityResult> {
    job.output
        .as_ref()
        .and_then(|output| serde_json::from_value(output.clone()).ok())
}

fn runtime_job_terminal_state(job: &RuntimeJob) -> Option<String> {
    match job.status {
        RuntimeJobStatus::Succeeded | RuntimeJobStatus::Failed | RuntimeJobStatus::Cancelled => {
            Some(eval_runtime_job_status(job.status).to_string())
        }
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

fn usage_snapshots(
    workflow_id: Option<&str>,
    runtime_events: &BTreeMap<String, Vec<RuntimeEvent>>,
) -> Vec<UsageSnapshot> {
    let mut usage = Vec::new();
    for (runtime_job_id, events) in runtime_events {
        for event in events {
            if !matches!(
                event.event_type.as_str(),
                "UsageRecorded" | "TokenUsageRecorded"
            ) {
                continue;
            }
            let snapshot = usage_snapshot_from_event(workflow_id, runtime_job_id, &event.event);
            if usage_snapshot_has_measurement(&snapshot) {
                usage.push(snapshot);
            }
        }
    }
    usage
}

fn usage_snapshot_from_event(
    workflow_id: Option<&str>,
    runtime_job_id: &str,
    event: &Value,
) -> UsageSnapshot {
    let payload = event.get("usage").unwrap_or(event);
    let input_tokens = first_u64_field(payload, &["input_tokens", "input"]);
    let output_tokens = first_u64_field(payload, &["output_tokens", "output"]);
    let cached_input_tokens = first_u64_field(
        payload,
        &[
            "cached_input_tokens",
            "cache_read_input_tokens",
            "cache_creation_input_tokens",
        ],
    );
    let total_tokens = first_u64_field(payload, &["total_tokens"]).or_else(|| {
        (input_tokens.is_some() || output_tokens.is_some()).then(|| {
            input_tokens
                .unwrap_or(0)
                .saturating_add(output_tokens.unwrap_or(0))
        })
    });
    let cost_usd_micros = first_u64_field(payload, &["cost_usd_micros"]);
    UsageSnapshot {
        agent_invocation_id: payload
            .get("agent_invocation_id")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        runtime_job_id: Some(runtime_job_id.to_string()),
        workflow_id: workflow_id.map(ToOwned::to_owned),
        model: payload
            .get("model")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        reasoning_effort: payload
            .get("reasoning_effort")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        input_tokens,
        output_tokens,
        cached_input_tokens,
        total_tokens,
        cost_usd_micros,
        token_confidence: Confidence::Observed,
        cost_confidence: if cost_usd_micros.is_some() {
            Confidence::Estimated
        } else {
            Confidence::Unknown
        },
    }
}

fn first_u64_field(value: &Value, keys: &[&str]) -> Option<u64> {
    keys.iter().find_map(|key| value.get(*key)?.as_u64())
}

fn usage_snapshot_has_measurement(snapshot: &UsageSnapshot) -> bool {
    snapshot.input_tokens.is_some()
        || snapshot.output_tokens.is_some()
        || snapshot.cached_input_tokens.is_some()
        || snapshot.total_tokens.is_some()
        || snapshot.cost_usd_micros.is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::{
        ActivityResult, RuntimeKind, RuntimeProfile, ValidationRecord, WorkflowCommand,
        WorkflowCommandStatus, WorkflowSubject, GITHUB_ISSUE_PR_DEFINITION_ID,
    };
    use serde_json::json;

    #[test]
    fn eval_evidence_missing_quality_gate_and_usage_fails_case() {
        let workflow = WorkflowInstance::new(
            GITHUB_ISSUE_PR_DEFINITION_ID,
            1,
            "pr_open",
            WorkflowSubject::new("issue", "issue:42"),
        )
        .with_id("workflow-1")
        .with_data(json!({
            "repo": "owner/repo",
            "issue_number": 42,
            "task_id": "eval-task-1",
            "eval": {"eval_run_id": "run-1", "case_id": "owner/repo#42"}
        }));
        let command = command_record(
            "cmd-1",
            "workflow-1",
            WorkflowCommand::enqueue_activity("implement_issue", "impl-1"),
            WorkflowCommandStatus::Completed,
        );
        let mut job = RuntimeJob::pending(
            "cmd-1",
            RuntimeKind::CodexExec,
            RuntimeProfile::new("codex", RuntimeKind::CodexExec).name,
            json!({"activity": "implement_issue"}),
        );
        job.id = "job-1".to_string();
        job.complete(
            &ActivityResult::succeeded("implement_issue", "opened PR").with_artifact(
                crate::runtime::ActivityArtifact::new(
                    "pull_request",
                    json!({"pr_number": 5, "pr_url": "https://github.com/owner/repo/pull/5"}),
                ),
            ),
        )
        .expect("complete job");

        let evidence = collect_eval_case_evidence_from_records(
            "run-1",
            "owner/repo#42",
            Some(&workflow),
            &[command],
            &[job],
            &BTreeMap::new(),
        );

        assert_eq!(evidence.status, EvalEvidenceStatus::Failed);
        assert!(evidence
            .missing_evidence
            .contains(&"quality_gate".to_string()));
        assert!(evidence.missing_evidence.contains(&"usage".to_string()));
        assert_eq!(
            evidence.submission.as_ref().unwrap().runtime_job_ids,
            vec!["job-1".to_string()]
        );
    }

    #[test]
    fn eval_evidence_maps_quality_gate_and_usage_records() {
        let workflow = WorkflowInstance::new(
            GITHUB_ISSUE_PR_DEFINITION_ID,
            1,
            "done",
            WorkflowSubject::new("issue", "issue:42"),
        )
        .with_id("workflow-1")
        .with_data(json!({
            "repo": "owner/repo",
            "issue_number": 42,
            "task_id": "eval-task-1",
            "eval": {"eval_run_id": "run-1", "case_id": "owner/repo#42"}
        }));
        let implementation = command_record(
            "cmd-impl",
            "workflow-1",
            WorkflowCommand::enqueue_activity("implement_issue", "impl-1"),
            WorkflowCommandStatus::Completed,
        );
        let quality = command_record(
            "cmd-quality",
            "workflow-1",
            WorkflowCommand::enqueue_activity(QUALITY_GATE_ACTIVITY, "quality-1"),
            WorkflowCommandStatus::Completed,
        );
        let mut implementation_job = RuntimeJob::pending(
            "cmd-impl",
            RuntimeKind::CodexExec,
            "codex",
            json!({"activity": "implement_issue"}),
        );
        implementation_job.id = "job-impl".to_string();
        implementation_job
            .complete(
                &ActivityResult::succeeded("implement_issue", "opened PR").with_artifact(
                    crate::runtime::ActivityArtifact::new(
                        "pull_request",
                        json!({"pr_number": 5, "pr_url": "https://github.com/owner/repo/pull/5"}),
                    ),
                ),
            )
            .expect("complete implementation job");
        let mut quality_job = RuntimeJob::pending(
            "cmd-quality",
            RuntimeKind::CodexExec,
            "codex",
            json!({"activity": QUALITY_GATE_ACTIVITY}),
        );
        quality_job.id = "job-quality".to_string();
        quality_job
            .complete(
                &ActivityResult::succeeded(QUALITY_GATE_ACTIVITY, "validation passed")
                    .with_validation(ValidationRecord::new(
                        "cargo test -p harness-workflow eval_evidence",
                        "passed",
                    )),
            )
            .expect("complete quality job");
        let mut events = BTreeMap::new();
        events.insert(
            "job-impl".to_string(),
            vec![RuntimeEvent::new(
                "job-impl",
                1,
                "UsageRecorded",
                json!({
                    "usage": {
                        "agent_invocation_id": "agent-1",
                        "model": "codex-test",
                        "input_tokens": 100,
                        "output_tokens": 20,
                        "total_tokens": 120,
                        "cost_usd_micros": 50
                    }
                }),
            )],
        );

        let evidence = collect_eval_case_evidence_from_records(
            "run-1",
            "owner/repo#42",
            Some(&workflow),
            &[implementation, quality],
            &[implementation_job, quality_job],
            &events,
        );

        assert_eq!(evidence.status, EvalEvidenceStatus::Passed);
        assert!(evidence.missing_evidence.is_empty());
        assert_eq!(
            evidence.quality_gate.as_ref().unwrap().validation_commands,
            vec!["cargo test -p harness-workflow eval_evidence".to_string()]
        );
        assert_eq!(evidence.usage[0].total_tokens, Some(120));
        assert_eq!(
            evidence.runtime.as_ref().unwrap().terminal_state,
            Some("done".to_string())
        );
    }

    #[test]
    fn eval_usage_invalid_cost_keeps_cost_confidence_unknown() {
        let snapshot = usage_snapshot_from_event(
            Some("workflow-1"),
            "job-1",
            &json!({
                "usage": {
                    "input_tokens": 10,
                    "cost_usd_micros": "not-a-number"
                }
            }),
        );

        assert_eq!(snapshot.cost_usd_micros, None);
        assert_eq!(snapshot.cost_confidence, Confidence::Unknown);
    }

    fn command_record(
        id: &str,
        workflow_id: &str,
        command: WorkflowCommand,
        status: WorkflowCommandStatus,
    ) -> WorkflowCommandRecord {
        WorkflowCommandRecord {
            id: id.to_string(),
            workflow_id: workflow_id.to_string(),
            decision_id: None,
            status,
            dispatch_owner: None,
            dispatch_lease_expires_at: None,
            command,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }
}
