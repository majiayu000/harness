use super::*;
use crate::handlers::runtime_hosts::applied_eval_network_policy;
use harness_sandbox::EvalNetworkPolicy;

pub(in crate::handlers::runtime_hosts) fn eval_network_policy_preflight_failure(
    job: &RuntimeJob,
    error: &str,
    network_policy: Option<EvalNetworkPolicy>,
) -> ActivityResult {
    let mut artifact = json!({
        "enforced": false,
        "reason": error,
    });
    if let Some(network_policy) = network_policy {
        artifact["network_policy"] = json!(network_policy);
    }
    ActivityResult::failed(
        super::evidence::runtime_job_activity(job),
        "Evaluation network policy could not be enforced.",
        error,
    )
    .with_error_kind(ActivityErrorKind::Configuration)
    .with_artifact(ActivityArtifact::new(
        "network_policy_enforcement",
        artifact,
    ))
}

pub(in crate::handlers::runtime_hosts) fn validate_eval_network_policy_report(
    job: &RuntimeJob,
    result: &ActivityResult,
) -> Result<(), (StatusCode, serde_json::Value)> {
    // Only require a report for leases that received the policy contract at claim time.
    let Some(expected_policy) = applied_eval_network_policy(job).map_err(|error| {
        (
            StatusCode::BAD_REQUEST,
            json!({ "error": format!("invalid eval network policy: {error}") }),
        )
    })?
    else {
        return Ok(());
    };

    let report_artifacts = result
        .artifacts
        .iter()
        .filter(|artifact| artifact.artifact_type == "network_policy_report")
        .collect::<Vec<_>>();
    if report_artifacts.len() > 1 {
        return Err((
            StatusCode::BAD_REQUEST,
            json!({
                "error": "eval runtime job completion must include exactly one network_policy_report artifact"
            }),
        ));
    }
    let Some(report_value) = report_artifacts
        .first()
        .map(|artifact| artifact.artifact.clone())
    else {
        return Err((
            StatusCode::BAD_REQUEST,
            json!({
                "error": "eval runtime job completion requires network_policy_report artifact"
            }),
        ));
    };
    let report: harness_sandbox::EvalNetworkPolicyReport = serde_json::from_value(report_value)
        .map_err(|error| {
            (
                StatusCode::BAD_REQUEST,
                json!({ "error": format!("invalid network_policy_report artifact: {error}") }),
            )
        })?;
    report
        .validate_against(&expected_policy, &job.id)
        .map_err(|error| {
            (
                StatusCode::BAD_REQUEST,
                json!({ "error": format!("network_policy_report is not valid: {error}") }),
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_workflow::runtime::RuntimeKind;

    fn eval_job_with_applied_policy(job_id_seed: &str) -> RuntimeJob {
        let mut job = RuntimeJob::pending(
            job_id_seed,
            RuntimeKind::RemoteHost,
            "remote-host-default",
            json!({
                "activity": "implement_issue",
                "command": {
                    "eval": {
                        "eval_run_id": "run-1",
                        "case_id": "case-1",
                        "timeout_secs": 45,
                        "network_policy": {
                            "inbound": "deny",
                            "outbound": "deny",
                            "network_allowlist": [],
                        }
                    }
                }
            }),
        );
        job.id = format!("job-{job_id_seed}");
        job
    }

    #[test]
    fn eval_network_policy_report_skipped_without_claim_contract() {
        let job = RuntimeJob::pending(
            "cmd-1",
            RuntimeKind::RemoteHost,
            "remote-host-default",
            json!({
                "activity": "implement_issue",
                "command": {
                    "eval": {
                        "eval_run_id": "run-1",
                        "case_id": "case-1",
                        "timeout_secs": 45
                    }
                }
            }),
        );
        let result = ActivityResult::succeeded("implement_issue", "done");
        validate_eval_network_policy_report(&job, &result)
            .expect("legacy leases without applied policy must not require a report");
    }

    #[test]
    fn eval_network_policy_report_is_required_when_policy_was_applied() {
        let job = eval_job_with_applied_policy("required");
        let result = ActivityResult::succeeded("implement_issue", "done");
        let err = validate_eval_network_policy_report(&job, &result)
            .expect_err("missing network report should fail closed");
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
        assert_eq!(
            err.1["error"],
            "eval runtime job completion requires network_policy_report artifact"
        );
    }

    #[test]
    fn eval_network_policy_report_accepts_matching_metadata() {
        let job = eval_job_with_applied_policy("accept");
        let result = ActivityResult::succeeded("implement_issue", "done").with_artifact(
            ActivityArtifact::new(
                "network_policy_report",
                json!({
                    "runtime_job_id": job.id,
                    "enforced": true,
                    "policy": {
                        "inbound": "deny",
                        "outbound": "deny",
                        "network_allowlist": [],
                    },
                    "grants": [],
                    "connections": [],
                    "payloads_recorded": false,
                    "reason": "runtime host enforced eval network policy",
                }),
            ),
        );
        validate_eval_network_policy_report(&job, &result)
            .expect("matching network policy report should be accepted");
    }

    #[test]
    fn eval_network_policy_report_rejects_payload_recording() {
        let job = eval_job_with_applied_policy("payload");
        let result = ActivityResult::succeeded("implement_issue", "done").with_artifact(
            ActivityArtifact::new(
                "network_policy_report",
                json!({
                    "runtime_job_id": job.id,
                    "enforced": true,
                    "policy": {
                        "inbound": "deny",
                        "outbound": "deny",
                        "network_allowlist": [],
                    },
                    "grants": [],
                    "connections": [],
                    "payloads_recorded": true,
                    "reason": "runtime host recorded network details",
                }),
            ),
        );
        let err = validate_eval_network_policy_report(&job, &result)
            .expect_err("payload recording must fail closed");
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
        assert!(err.1["error"]
            .as_str()
            .is_some_and(|error| error.contains("must not record payloads")));
    }

    #[test]
    fn eval_network_policy_report_rejects_duplicates() {
        let job = eval_job_with_applied_policy("dup");
        let result = ActivityResult::succeeded("implement_issue", "done")
            .with_artifact(ActivityArtifact::new(
                "network_policy_report",
                json!({
                    "runtime_job_id": job.id,
                    "enforced": true,
                    "policy": {
                        "inbound": "deny",
                        "outbound": "deny",
                        "network_allowlist": [],
                    },
                    "grants": [],
                    "connections": [],
                    "payloads_recorded": false,
                    "reason": "runtime host enforced eval network policy",
                }),
            ))
            .with_artifact(ActivityArtifact::new(
                "network_policy_report",
                json!({
                    "runtime_job_id": job.id,
                    "enforced": true,
                    "policy": {
                        "inbound": "deny",
                        "outbound": "deny",
                        "network_allowlist": [],
                    },
                    "grants": [],
                    "connections": [],
                    "payloads_recorded": true,
                    "reason": "contradictory report",
                }),
            ));
        let err = validate_eval_network_policy_report(&job, &result)
            .expect_err("duplicate reports must fail closed");
        assert!(err.1["error"]
            .as_str()
            .is_some_and(|error| error.contains("exactly one")));
    }
}
