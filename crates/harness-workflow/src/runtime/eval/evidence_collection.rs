use super::evidence::{
    activity_result_from_job, collect_eval_case_evidence_from_records, EvalCaseEvidence,
    EvalEvidenceStatus,
};
use crate::runtime::completion_evidence::ARTIFACT_EVAL_BASE_CHECKOUT;
use crate::runtime::{RuntimeJob, WorkflowInstance, WorkflowRuntimeStore};
use serde_json::Value;

pub async fn collect_eval_case_evidence(
    store: &WorkflowRuntimeStore,
    eval_run_id: &str,
    case_id: &str,
    workflow_id: &str,
    expected_base_commit: &str,
) -> anyhow::Result<EvalCaseEvidence> {
    let workflow = store.get_instance(workflow_id).await?;
    let workflows = workflow_family(store, workflow_id).await?;
    let workflow_ids = workflows
        .iter()
        .map(|workflow| workflow.id.clone())
        .collect::<Vec<_>>();
    let commands = store
        .commands_for_workflows(&workflow_ids)
        .await?
        .into_values()
        .flatten()
        .collect::<Vec<_>>();
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

    let mut evidence = collect_eval_case_evidence_from_records(
        eval_run_id,
        case_id,
        workflow.as_ref(),
        &commands,
        &runtime_jobs,
        &events_by_job,
    );
    apply_base_checkout_evidence(&mut evidence, &runtime_jobs, expected_base_commit);
    apply_persisted_cleanup_evidence(store, workflow_id, &mut evidence).await?;
    if let Some(attestation) = collected_attestation(&runtime_jobs) {
        evidence.attestation = attestation;
    }
    evidence.status = collected_evidence_status(&evidence.missing_evidence, &runtime_jobs);
    Ok(evidence)
}

fn collected_attestation(
    runtime_jobs: &[RuntimeJob],
) -> Option<super::super::EvalAttestationSummary> {
    runtime_jobs
        .iter()
        .filter_map(activity_result_from_job)
        .flat_map(|result| result.artifacts)
        .filter(|artifact| artifact.artifact_type == "eval_run_attestation_summary")
        .find_map(|artifact| serde_json::from_value(artifact.artifact).ok())
}

fn collected_evidence_status(
    missing: &[String],
    runtime_jobs: &[RuntimeJob],
) -> EvalEvidenceStatus {
    if missing.is_empty() {
        return EvalEvidenceStatus::Passed;
    }
    if runtime_jobs
        .iter()
        .filter_map(activity_result_from_job)
        .any(|result| {
            matches!(
                result.error_kind,
                Some(
                    crate::runtime::ActivityErrorKind::Retryable
                        | crate::runtime::ActivityErrorKind::SpawnFailure
                        | crate::runtime::ActivityErrorKind::Configuration
                        | crate::runtime::ActivityErrorKind::ExternalDependency
                        | crate::runtime::ActivityErrorKind::Unknown
                )
            )
        })
    {
        return EvalEvidenceStatus::EvidenceIncomplete;
    }
    if missing
        .iter()
        .all(|key| matches!(key.as_str(), "quality_gate" | "quality_gate_pass"))
    {
        return EvalEvidenceStatus::Failed;
    }
    EvalEvidenceStatus::EvidenceIncomplete
}

async fn workflow_family(
    store: &WorkflowRuntimeStore,
    root_workflow_id: &str,
) -> anyhow::Result<Vec<WorkflowInstance>> {
    let mut family = Vec::new();
    let mut pending = vec![root_workflow_id.to_string()];
    while let Some(workflow_id) = pending.pop() {
        let Some(instance) = store.get_instance(&workflow_id).await? else {
            continue;
        };
        pending.extend(
            store
                .list_instances_by_parent(&workflow_id, None)
                .await?
                .into_iter()
                .map(|child| child.id),
        );
        family.push(instance);
    }
    Ok(family)
}

fn apply_base_checkout_evidence(
    evidence: &mut EvalCaseEvidence,
    runtime_jobs: &[RuntimeJob],
    expected_base_commit: &str,
) {
    let checkout = runtime_jobs
        .iter()
        .filter_map(activity_result_from_job)
        .flat_map(|result| result.artifacts)
        .find(|artifact| artifact.artifact_type == ARTIFACT_EVAL_BASE_CHECKOUT);
    let Some(checkout) = checkout else {
        evidence
            .missing_evidence
            .push("base_commit_verification".to_string());
        return;
    };
    let requested = checkout
        .artifact
        .get("requested_commit")
        .and_then(Value::as_str);
    let observed = checkout
        .artifact
        .get("observed_commit")
        .and_then(Value::as_str);
    if requested.is_some_and(|commit| commit.eq_ignore_ascii_case(expected_base_commit))
        && observed.is_some_and(|commit| {
            commit
                .to_ascii_lowercase()
                .starts_with(&expected_base_commit.to_ascii_lowercase())
        })
    {
        // The manifest accepts abbreviated hashes; a full observed hash that starts
        // with the requested hash proves the runtime used the pinned revision.
    } else {
        evidence.missing_evidence.push(format!(
            "base_commit_mismatch: expected {expected_base_commit}, requested {}, observed {}",
            requested.unwrap_or("<missing>"),
            observed.unwrap_or("<missing>")
        ));
    }
}

async fn apply_persisted_cleanup_evidence(
    store: &WorkflowRuntimeStore,
    workflow_id: &str,
    evidence: &mut EvalCaseEvidence,
) -> anyhow::Result<()> {
    let Some(cleanup) = store
        .latest_event_for_type(workflow_id, "EvalCaseCleanupCompleted")
        .await?
    else {
        return Ok(());
    };
    let status = cleanup.event.get("status").and_then(Value::as_str);
    if let Some(isolation) = evidence.isolation.as_mut() {
        isolation.cleanup_status = cleanup
            .event
            .get("status")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned);
    }
    if status == Some("cleaned") {
        evidence
            .missing_evidence
            .retain(|missing| missing != "isolation_cleanup");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::{
        ActivityArtifact, ActivityResult, ActivityStatus, RuntimeJob, RuntimeKind,
    };
    use serde_json::json;

    fn evidence() -> EvalCaseEvidence {
        EvalCaseEvidence {
            eval_run_id: "run-1".to_string(),
            case_id: "case-1".to_string(),
            workflow_id: Some("workflow-1".to_string()),
            status: EvalEvidenceStatus::Failed,
            attestation: super::super::EvalAttestationSummary::unsigned(),
            runtime: None,
            usage: Vec::new(),
            submission: None,
            quality_gate: None,
            quality: None,
            isolation: None,
            missing_evidence: Vec::new(),
        }
    }

    #[test]
    fn verification_failure_is_scored_but_missing_runtime_evidence_is_infrastructure() {
        assert_eq!(
            collected_evidence_status(&["quality_gate_pass".to_string()], &[]),
            EvalEvidenceStatus::Failed
        );
        assert_eq!(
            collected_evidence_status(&["usage".to_string()], &[]),
            EvalEvidenceStatus::EvidenceIncomplete
        );

        for error_kind in [
            crate::runtime::ActivityErrorKind::Configuration,
            crate::runtime::ActivityErrorKind::SpawnFailure,
        ] {
            let mut job =
                RuntimeJob::pending("command", RuntimeKind::RemoteHost, "host", json!({}));
            assert!(job
                .complete(
                    &ActivityResult::failed("run_quality_gate", "failed", "infrastructure")
                        .with_error_kind(error_kind),
                )
                .is_ok());
            assert_eq!(
                collected_evidence_status(&["quality_gate_pass".to_string()], &[job]),
                EvalEvidenceStatus::EvidenceIncomplete
            );
        }
    }

    #[test]
    fn collected_attestation_cannot_promote_an_unverified_summary() {
        let mut job = RuntimeJob::pending("command", RuntimeKind::RemoteHost, "host", json!({}));
        let result = ActivityResult::succeeded("implement_issue", "done").with_artifact(
            crate::runtime::ActivityArtifact::new(
                "eval_run_attestation_summary",
                json!({"trust": "verified", "provider": "untrusted-host"}),
            ),
        );
        assert!(job.complete(&result).is_ok());

        assert_eq!(
            collected_attestation(&[job]).map(|summary| summary.trust()),
            Some(super::super::EvalAttestationTrust::Unverified)
        );
    }

    fn checkout_job(requested: &str, observed: &str) -> RuntimeJob {
        let mut job = RuntimeJob::pending(
            "command-1",
            RuntimeKind::RemoteHost,
            "eval-host",
            json!({"activity": "implement_issue"}),
        );
        job.output = Some(
            serde_json::to_value(ActivityResult {
                activity: "implement_issue".to_string(),
                status: ActivityStatus::Succeeded,
                summary: "done".to_string(),
                artifacts: vec![ActivityArtifact::new(
                    ARTIFACT_EVAL_BASE_CHECKOUT,
                    json!({
                        "requested_commit": requested,
                        "observed_commit": observed,
                    }),
                )],
                signals: Vec::new(),
                validation: Vec::new(),
                error: None,
                error_kind: None,
            })
            .expect("activity result should serialize"),
        );
        job
    }

    #[test]
    fn base_checkout_accepts_a_full_observed_hash_for_a_short_manifest_hash() {
        let mut evidence = evidence();
        apply_base_checkout_evidence(
            &mut evidence,
            &[checkout_job("abcdef1", "abcdef1234567890")],
            "abcdef1",
        );

        assert!(evidence.missing_evidence.is_empty());
    }

    #[test]
    fn base_checkout_fails_closed_on_a_mismatched_observed_hash() {
        let mut evidence = evidence();
        apply_base_checkout_evidence(
            &mut evidence,
            &[checkout_job("abcdef1", "1234567890abcdef")],
            "abcdef1",
        );

        assert!(evidence.missing_evidence[0].starts_with("base_commit_mismatch:"));
    }

    #[test]
    fn base_checkout_compares_hexadecimal_commits_case_insensitively() {
        let mut evidence = evidence();
        apply_base_checkout_evidence(
            &mut evidence,
            &[checkout_job("abcdef1", "abcdef1234567890")],
            "ABCDEF1",
        );

        assert!(evidence.missing_evidence.is_empty());
    }
}
