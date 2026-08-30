use super::*;

pub(super) fn is_eval_cancellation_ack(job: &RuntimeJob, result: &ActivityResult) -> bool {
    result.status == harness_workflow::runtime::ActivityStatus::Cancelled
        && job.input.get("cancellation_requested").is_some()
        && job.is_eval_job()
}

pub(super) fn attach_eval_cancellation_cleanup_evidence(
    result: ActivityResult,
    execution_evidence: Option<RuntimeHostExecutionEvidence>,
) -> Result<ActivityResult, serde_json::Value> {
    let Some(execution_evidence) = execution_evidence else {
        return Err(json!({"error": "eval cancellation requires host cleanup evidence"}));
    };
    if execution_evidence.isolation_cleanup_status.trim() != "cleaned" {
        return Err(json!({"error": "eval cancellation cleanup is not confirmed"}));
    }
    Ok(result.with_artifact(ActivityArtifact::new(
        harness_workflow::runtime::completion_evidence::ARTIFACT_EVAL_ISOLATION_CLEANUP,
        json!({
            "status": "cleaned",
            "evidence_source": "runtime_host_cancellation_ack",
        }),
    )))
}

pub(in crate::handlers::runtime_hosts) fn eval_resource_limit_preflight_failure(
    job: &RuntimeJob,
    error: &str,
    resource_limits: Option<CappedResourceLimits>,
) -> ActivityResult {
    let mut artifact = json!({
        "enforced": false,
        "reason": error,
    });
    if let Some(resource_limits) = resource_limits {
        artifact["resource_limits"] = json!(resource_limits);
    }
    ActivityResult::failed(
        runtime_job_activity(job),
        "Evaluation resource limits could not be enforced.",
        error,
    )
    .with_error_kind(ActivityErrorKind::Configuration)
    .with_artifact(ActivityArtifact::new(
        "resource_limit_enforcement",
        artifact,
    ))
}

fn runtime_job_activity(job: &RuntimeJob) -> String {
    job.input
        .get("activity")
        .and_then(Value::as_str)
        .or_else(|| {
            job.input
                .pointer("/command/activity")
                .and_then(Value::as_str)
        })
        .unwrap_or("remote_host")
        .to_string()
}

pub(super) fn attach_eval_checkout_evidence(
    job: &RuntimeJob,
    result: ActivityResult,
    execution_evidence: Option<RuntimeHostExecutionEvidence>,
) -> Result<ActivityResult, serde_json::Value> {
    let Some((expected, exact_match)) = expected_eval_checkout_commit(job) else {
        return Ok(result);
    };
    let Some(execution_evidence) = execution_evidence else {
        return Err(json!({
            "error": "eval completion requires host execution_evidence"
        }));
    };
    let observed = execution_evidence
        .checked_out_commit
        .trim()
        .to_ascii_lowercase();
    let expected = expected.to_ascii_lowercase();
    if observed.len() != 40
        || !observed
            .chars()
            .all(|character| character.is_ascii_hexdigit())
        || if exact_match {
            observed != expected
        } else {
            !observed.starts_with(&expected)
        }
    {
        return Err(json!({
            "error": "checked_out_commit does not match the requested eval commit",
            "expected_commit": expected,
            "observed_commit": observed,
        }));
    }
    let usage = &execution_evidence.usage;
    let model = usage.model.trim();
    let measured_total = usage.input_tokens.saturating_add(usage.output_tokens);
    if model.is_empty()
        || usage.cached_input_tokens > usage.input_tokens
        || usage.total_tokens < measured_total
        || usage.total_tokens == 0
    {
        return Err(json!({
            "error": "eval host usage evidence is invalid",
            "measured_total_tokens": measured_total,
            "reported_total_tokens": usage.total_tokens,
        }));
    }
    if execution_evidence.isolation_cleanup_status.trim() != "cleaned" {
        return Err(json!({
            "error": "eval host execution evidence must confirm ephemeral isolation cleanup"
        }));
    }
    let report: ResourceLimitReport = serde_json::from_value(
        execution_evidence.resource_limit_report.clone(),
    )
    .map_err(|error| json!({ "error": format!("invalid host resource_limit_report: {error}") }))?;

    let result = result
        .with_artifact(ActivityArtifact::new(
            harness_workflow::runtime::completion_evidence::ARTIFACT_EVAL_BASE_CHECKOUT,
            json!({
                "requested_commit": expected,
                "observed_commit": observed,
                "evidence_source": "runtime_host_completion_request",
            }),
        ))
        .with_artifact(ActivityArtifact::new(
            harness_workflow::runtime::completion_evidence::ARTIFACT_RESOURCE_LIMIT_REPORT,
            json!(report),
        ))
        .with_artifact(ActivityArtifact::new(
            harness_workflow::runtime::completion_evidence::ARTIFACT_RUNTIME_HOST_USAGE,
            json!({
                "model": model,
                "input_tokens": usage.input_tokens,
                "output_tokens": usage.output_tokens,
                "cached_input_tokens": usage.cached_input_tokens,
                "total_tokens": usage.total_tokens,
                "cost_usd_micros": usage.cost_usd_micros,
                "evidence_source": "runtime_host_completion_request",
            }),
        ))
        .with_artifact(ActivityArtifact::new(
            harness_workflow::runtime::completion_evidence::ARTIFACT_EVAL_ISOLATION_CLEANUP,
            json!({
                "status": "cleaned",
                "evidence_source": "runtime_host_completion_request",
            }),
        ));
    attach_eval_host_validation_evidence(job, result, &execution_evidence.validation)
}

fn attach_eval_host_validation_evidence(
    job: &RuntimeJob,
    mut result: ActivityResult,
    validation: &[harness_protocol::rest::RuntimeHostValidationEvidence],
) -> Result<ActivityResult, serde_json::Value> {
    if runtime_job_activity(job) != harness_workflow::runtime::QUALITY_GATE_ACTIVITY {
        return Ok(result);
    }
    let expected = job
        .input
        .pointer("/command/validation_commands_argv")
        .and_then(Value::as_array)
        .ok_or_else(|| json!({"error": "eval quality gate requires validation command argv"}))?;
    if expected.len() != validation.len() || expected.is_empty() {
        return Err(json!({"error": "host validation evidence does not match expected commands"}));
    }
    let mut digest = Vec::with_capacity(validation.len());
    let mut validation_failed = false;
    result.validation.clear();
    for (expected, observed) in expected.iter().zip(validation) {
        let expected = serde_json::from_value::<Vec<String>>(expected.clone())
            .map_err(|_| json!({"error": "eval quality gate validation argv is invalid"}))?;
        if observed.argv != expected
            || observed.output_sha256.len() != 64
            || !observed
                .output_sha256
                .chars()
                .all(|character| character.is_ascii_hexdigit())
        {
            return Err(json!({"error": "host validation evidence is invalid"}));
        }
        let display = shlex::try_join(observed.argv.iter().map(String::as_str))
            .map_err(|_| json!({"error": "host validation argv cannot be represented"}))?;
        result
            .validation
            .push(harness_workflow::runtime::ValidationRecord::new(
                &display,
                if observed.exit_code == 0 {
                    "passed"
                } else {
                    "failed"
                },
            ));
        validation_failed |= observed.exit_code != 0;
        digest.push(json!({
            "command": display,
            "argv": observed.argv,
            "exit_code": observed.exit_code,
            "output_sha256": observed.output_sha256,
            "duration_ms": observed.duration_ms,
            "evidence_source": "runtime_host_completion_request",
        }));
    }
    if validation_failed {
        result.status = harness_workflow::runtime::ActivityStatus::Failed;
        result.summary = "One or more revision-bound validation commands failed.".to_string();
        result.error = Some("revision-bound validation command failed".to_string());
        result.error_kind = Some(harness_workflow::runtime::ActivityErrorKind::Fatal);
    }
    Ok(result.with_artifact(ActivityArtifact::new(
        harness_workflow::runtime::completion_evidence::ARTIFACT_SERVER_VALIDATION_DIGEST,
        json!({"commands": digest}),
    )))
}

fn expected_eval_checkout_commit(job: &RuntimeJob) -> Option<(&str, bool)> {
    let eval = eval_metadata(&job.input)?;
    let (commit, exact) = match runtime_job_activity(job).as_str() {
        "implement_issue" => (eval.get("base_commit")?, false),
        harness_workflow::runtime::QUALITY_GATE_ACTIVITY => {
            (job.input.pointer("/command/expected_head_sha")?, true)
        }
        _ => return None,
    };
    commit
        .as_str()
        .map(str::trim)
        .filter(|commit| !commit.is_empty())
        .map(|commit| (commit, exact))
}

pub(in crate::handlers::runtime_hosts) fn validate_eval_resource_limit_report(
    job: &RuntimeJob,
    result: &ActivityResult,
) -> Result<(), (StatusCode, serde_json::Value)> {
    let Some(expected_limits) = eval_resource_limit_enforcement_for_job(job).map_err(|error| {
        (
            StatusCode::BAD_REQUEST,
            json!({ "error": format!("invalid eval resource limits: {error}") }),
        )
    })?
    else {
        return Ok(());
    };

    let Some(report_value) = result
        .artifacts
        .iter()
        .find(|artifact| artifact.artifact_type == "resource_limit_report")
        .map(|artifact| artifact.artifact.clone())
    else {
        return Err((
            StatusCode::BAD_REQUEST,
            json!({
                "error": "eval runtime job completion requires resource_limit_report artifact"
            }),
        ));
    };
    let report: ResourceLimitReport = serde_json::from_value(report_value).map_err(|error| {
        (
            StatusCode::BAD_REQUEST,
            json!({ "error": format!("invalid resource_limit_report artifact: {error}") }),
        )
    })?;
    if report.limits.effective != expected_limits.effective {
        return Err((
            StatusCode::BAD_REQUEST,
            json!({
                "error": "resource_limit_report limits do not match claimed eval resource limits"
            }),
        ));
    }
    if !resource_usage_covers_limits(&report.usage, &expected_limits.effective) {
        return Err((
            StatusCode::BAD_REQUEST,
            json!({
                "error": "resource_limit_report requires usage evidence for every effective limit"
            }),
        ));
    }
    if report.reason.trim().is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            json!({
                "error": "resource_limit_report requires a non-empty reason"
            }),
        ));
    }
    if result.status == harness_workflow::runtime::ActivityStatus::Succeeded
        && (report.termination.is_some() || resource_usage_exceeds_limits(&report))
    {
        return Err((
            StatusCode::BAD_REQUEST,
            json!({
                "error": "successful eval completion contradicts resource-limit evidence"
            }),
        ));
    }
    Ok(())
}

fn resource_usage_exceeds_limits(report: &ResourceLimitReport) -> bool {
    let limits = report.limits.effective;
    exceeds_millis(report.usage.cpu_time_millis, limits.cpu_time_secs)
        || exceeds(report.usage.peak_memory_bytes, limits.memory_bytes)
        || exceeds(report.usage.peak_pids, limits.pids)
        || exceeds(report.usage.disk_bytes, limits.disk_bytes)
        || exceeds(report.usage.output_bytes, limits.output_bytes)
        || exceeds_millis(report.usage.wall_time_millis, limits.wall_time_secs)
}

fn exceeds(observed: Option<u64>, limit: Option<u64>) -> bool {
    matches!((observed, limit), (Some(observed), Some(limit)) if observed > limit)
}

fn exceeds_millis(observed: Option<u64>, limit_secs: Option<u64>) -> bool {
    matches!((observed, limit_secs), (Some(observed), Some(limit)) if observed > limit.saturating_mul(1_000))
}

fn resource_usage_covers_limits(
    usage: &harness_sandbox::ResourceUsage,
    limits: &harness_sandbox::ResourceLimits,
) -> bool {
    (limits.cpu_time_secs.is_none() || usage.cpu_time_millis.is_some())
        && (limits.memory_bytes.is_none() || usage.peak_memory_bytes.is_some())
        && (limits.pids.is_none() || usage.peak_pids.is_some())
        && (limits.disk_bytes.is_none() || usage.disk_bytes.is_some())
        && (limits.output_bytes.is_none() || usage.output_bytes.is_some())
        && (limits.wall_time_secs.is_none() || usage.wall_time_millis.is_some())
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_workflow::runtime::store::runtime_job_leases::RuntimeJobLeaseRenewalOutcome;
    use harness_workflow::runtime::RuntimeKind;
    use uuid::Uuid;

    #[test]
    fn eval_resource_limits_derive_from_timeout_when_missing() {
        let job = eval_implementation_job("abcdef1");
        let limits = eval_resource_limit_enforcement_for_job(&job)
            .expect("limits should parse")
            .expect("eval job should require limits");

        assert_eq!(limits.effective.wall_time_secs, Some(45));
        assert_eq!(limits.effective.cpu_time_secs, Some(45));
        assert_eq!(limits.effective.output_bytes, Some(64 * 1024 * 1024));
    }

    fn eval_implementation_job(base_commit: &str) -> RuntimeJob {
        RuntimeJob::pending(
            "cmd-1",
            RuntimeKind::RemoteHost,
            "remote-host-default",
            json!({
                "activity": "implement_issue",
                "command": {
                    "eval": {
                        "eval_run_id": "run-1",
                        "case_id": "case-1",
                        "base_commit": base_commit,
                        "timeout_secs": 45
                    }
                }
            }),
        )
    }

    fn eval_quality_gate_job(expected_head_sha: &str) -> RuntimeJob {
        RuntimeJob::pending(
            "cmd-quality",
            RuntimeKind::RemoteHost,
            "remote-host-default",
            json!({
                "activity": harness_workflow::runtime::QUALITY_GATE_ACTIVITY,
                "command": {
                    "eval": {"eval_run_id": "run-1", "case_id": "case-1"},
                    "expected_head_sha": expected_head_sha,
                    "validation_commands_argv": [["cargo", "check"]],
                }
            }),
        )
    }

    fn host_execution_evidence(checked_out_commit: &str) -> RuntimeHostExecutionEvidence {
        let limits = ResourceLimits::evaluation_defaults(45)
            .cap_by(ResourceLimits::operator_default_maxima())
            .expect("limits should cap");
        RuntimeHostExecutionEvidence {
            checked_out_commit: checked_out_commit.to_string(),
            resource_limit_report: json!(ResourceLimitReport {
                limits,
                usage: complete_resource_usage(),
                termination: None,
                reason: "completed within resource limits".to_string(),
            }),
            usage: harness_protocol::rest::RuntimeHostUsageEvidence {
                model: "test-model".to_string(),
                input_tokens: 10,
                output_tokens: 5,
                cached_input_tokens: 0,
                total_tokens: 15,
                cost_usd_micros: Some(20),
            },
            isolation_cleanup_status: "cleaned".to_string(),
            validation: vec![harness_protocol::rest::RuntimeHostValidationEvidence {
                argv: vec!["cargo".to_string(), "check".to_string()],
                exit_code: 0,
                output_sha256: "a".repeat(64),
                duration_ms: 10,
            }],
        }
    }

    fn complete_resource_usage() -> harness_sandbox::ResourceUsage {
        harness_sandbox::ResourceUsage {
            cpu_time_millis: Some(100),
            peak_memory_bytes: Some(1024),
            peak_pids: Some(2),
            disk_bytes: Some(4096),
            output_bytes: Some(128),
            wall_time_millis: Some(1_000),
        }
    }

    #[test]
    fn eval_resource_limit_report_is_required_for_eval_completion() {
        let job = eval_implementation_job("abcdef1");
        let result = ActivityResult::succeeded("implement_issue", "done");
        let err = validate_eval_resource_limit_report(&job, &result)
            .expect_err("missing resource report should fail closed");

        assert_eq!(err.0, StatusCode::BAD_REQUEST);
        assert_eq!(
            err.1["error"],
            "eval runtime job completion requires resource_limit_report artifact"
        );
    }

    #[test]
    fn eval_resource_limit_report_accepts_matching_usage_evidence() {
        let limits = ResourceLimits::evaluation_defaults(45)
            .cap_by(ResourceLimits::operator_default_maxima())
            .expect("limits should cap");
        let mut job = eval_implementation_job("abcdef1");
        job.input["command"]["eval"]["resource_limits"] = json!(limits.clone());
        let result = ActivityResult::succeeded("implement_issue", "done").with_artifact(
            ActivityArtifact::new(
                "resource_limit_report",
                json!(ResourceLimitReport {
                    limits,
                    usage: complete_resource_usage(),
                    termination: None,
                    reason: "completed within resource limits".to_string(),
                }),
            ),
        );

        validate_eval_resource_limit_report(&job, &result)
            .expect("matching resource report should be accepted");
    }

    #[test]
    fn eval_resource_limit_report_requires_usage_evidence_and_reason() {
        let limits = ResourceLimits::evaluation_defaults(45)
            .cap_by(ResourceLimits::operator_default_maxima())
            .expect("limits should cap");
        let mut job = eval_implementation_job("abcdef1");
        job.input["command"]["eval"]["resource_limits"] = json!(limits.clone());
        let empty_usage = ActivityResult::succeeded("implement_issue", "done").with_artifact(
            ActivityArtifact::new(
                "resource_limit_report",
                json!(ResourceLimitReport {
                    limits: limits.clone(),
                    usage: harness_sandbox::ResourceUsage::default(),
                    termination: None,
                    reason: "completed within resource limits".to_string(),
                }),
            ),
        );
        let err = validate_eval_resource_limit_report(&job, &empty_usage)
            .expect_err("empty usage should fail closed");
        assert_eq!(
            err.1["error"],
            "resource_limit_report requires usage evidence for every effective limit"
        );

        let empty_reason = ActivityResult::succeeded("implement_issue", "done").with_artifact(
            ActivityArtifact::new(
                "resource_limit_report",
                json!(ResourceLimitReport {
                    limits,
                    usage: complete_resource_usage(),
                    termination: None,
                    reason: " ".to_string(),
                }),
            ),
        );
        let err = validate_eval_resource_limit_report(&job, &empty_reason)
            .expect_err("empty reason should fail closed");
        assert_eq!(
            err.1["error"],
            "resource_limit_report requires a non-empty reason"
        );
    }

    #[test]
    fn eval_base_checkout_requires_host_execution_evidence() {
        let job = eval_implementation_job("abcdef1");
        let error = attach_eval_checkout_evidence(
            &job,
            ActivityResult::succeeded("implement_issue", "done"),
            None,
        )
        .expect_err("missing host evidence must fail closed");
        assert_eq!(
            error["error"],
            "eval completion requires host execution_evidence"
        );
    }

    #[test]
    fn eval_base_checkout_rejects_a_mismatched_host_commit() {
        let job = eval_implementation_job("abcdef1");
        let error = attach_eval_checkout_evidence(
            &job,
            ActivityResult::succeeded("implement_issue", "done"),
            Some(host_execution_evidence(
                "1234567890abcdef1234567890abcdef12345678",
            )),
        )
        .expect_err("mismatched host evidence must fail closed");
        assert_eq!(
            error["error"],
            "checked_out_commit does not match the requested eval commit"
        );
    }

    #[test]
    fn eval_quality_gate_requires_the_exact_snapshot_head() {
        let expected = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let job = eval_quality_gate_job(expected);
        let mismatch = attach_eval_checkout_evidence(
            &job,
            ActivityResult::succeeded(harness_workflow::runtime::QUALITY_GATE_ACTIVITY, "done"),
            Some(host_execution_evidence(
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaab",
            )),
        );
        assert!(mismatch.is_err());
        assert!(attach_eval_checkout_evidence(
            &job,
            ActivityResult::succeeded(harness_workflow::runtime::QUALITY_GATE_ACTIVITY, "done"),
            Some(host_execution_evidence(expected)),
        )
        .is_ok());
    }

    #[test]
    fn eval_quality_gate_mints_host_validation_digest() {
        let expected = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let result = attach_eval_checkout_evidence(
            &eval_quality_gate_job(expected),
            ActivityResult::succeeded(harness_workflow::runtime::QUALITY_GATE_ACTIVITY, "done"),
            Some(host_execution_evidence(expected)),
        )
        .expect("matching host evidence should be accepted");

        assert!(
            harness_workflow::runtime::completion_evidence::server_validation_digest_passed(
                &result
            )
        );
        assert_eq!(result.validation[0].command, "cargo check");
    }

    #[test]
    fn eval_quality_gate_nonzero_validation_is_a_benchmark_failure() {
        let expected = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let mut evidence = host_execution_evidence(expected);
        evidence.validation[0].exit_code = 1;
        let result = attach_eval_checkout_evidence(
            &eval_quality_gate_job(expected),
            ActivityResult::failed(
                harness_workflow::runtime::QUALITY_GATE_ACTIVITY,
                "host classified validation as infrastructure",
                "configuration",
            )
            .with_error_kind(harness_workflow::runtime::ActivityErrorKind::Configuration),
            Some(evidence),
        )
        .expect("valid host evidence should be collected");

        assert_eq!(
            result.error_kind,
            Some(harness_workflow::runtime::ActivityErrorKind::Fatal)
        );
        assert!(
            !harness_workflow::runtime::completion_evidence::server_validation_digest_passed(
                &result
            )
        );
    }

    #[test]
    fn eval_cancellation_ack_mints_cleanup_proof() {
        let mut job = eval_implementation_job("abcdef1");
        job.input["cancellation_requested"] = json!({"reason": "operator cancelled"});
        let result = ActivityResult::cancelled("implement_issue", "cancelled");
        assert!(is_eval_cancellation_ack(&job, &result));

        let result = attach_eval_cancellation_cleanup_evidence(
            result,
            Some(host_execution_evidence(
                "abcdef1234567890abcdef1234567890abcdef12",
            )),
        )
        .expect("cleaned host acknowledgement should be accepted");
        assert!(result.artifacts.iter().any(|artifact| {
            artifact.artifact_type
                == harness_workflow::runtime::completion_evidence::ARTIFACT_EVAL_ISOLATION_CLEANUP
                && artifact.artifact["status"] == "cleaned"
        }));
    }

    #[test]
    fn eval_base_checkout_mints_server_reserved_evidence() {
        let job = eval_implementation_job("abcdef1");
        let result = attach_eval_checkout_evidence(
            &job,
            ActivityResult::succeeded("implement_issue", "done"),
            Some(host_execution_evidence(
                "abcdef1234567890abcdef1234567890abcdef12",
            )),
        )
        .expect("matching host evidence should be accepted");
        assert!(result.artifacts.iter().any(|artifact| {
            artifact.artifact_type
                == harness_workflow::runtime::completion_evidence::ARTIFACT_EVAL_BASE_CHECKOUT
                && artifact.artifact["evidence_source"] == "runtime_host_completion_request"
        }));
        assert!(result.artifacts.iter().any(|artifact| {
            artifact.artifact_type
                == harness_workflow::runtime::completion_evidence::ARTIFACT_RUNTIME_HOST_USAGE
        }));
    }

    #[test]
    fn successful_eval_rejects_resource_usage_above_the_effective_limit() {
        let limits = ResourceLimits::evaluation_defaults(45)
            .cap_by(ResourceLimits::operator_default_maxima())
            .expect("limits should cap");
        let mut job = eval_implementation_job("abcdef1");
        job.input["command"]["eval"]["resource_limits"] = json!(limits.clone());
        let result = ActivityResult::succeeded("implement_issue", "done").with_artifact(
            ActivityArtifact::new(
                harness_workflow::runtime::completion_evidence::ARTIFACT_RESOURCE_LIMIT_REPORT,
                json!(ResourceLimitReport {
                    limits,
                    usage: harness_sandbox::ResourceUsage {
                        wall_time_millis: Some(46_000),
                        ..complete_resource_usage()
                    },
                    termination: None,
                    reason: "reported success".to_string(),
                }),
            ),
        );

        let error = validate_eval_resource_limit_report(&job, &result)
            .expect_err("contradictory successful usage must fail closed");
        assert_eq!(
            error.1["error"],
            "successful eval completion contradicts resource-limit evidence"
        );
    }

    #[tokio::test]
    async fn completion_reservation_reconciles_old_fence_and_deregister_revokes_final_commit(
    ) -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let Some((state, store)) =
            crate::handlers::runtime_hosts_workflow_api_tests::make_test_state_with_runtime_store(
                dir.path(),
            )
            .await?
        else {
            return Ok(());
        };
        let app =
            crate::handlers::runtime_hosts_workflow_api_tests::runtime_hosts_workflow_app(state);
        crate::handlers::runtime_hosts_workflow_api_tests::register_host(&app, "host-a").await?;
        let job = crate::handlers::runtime_hosts_workflow_api_tests::enqueue_runtime_host_test_job(
            store.as_ref(),
            "completion-reservation-reconcile",
            RuntimeKind::RemoteHost,
            "remote-host-default",
            json!({"activity": "remote_check"}),
        )
        .await?;
        let claimed = crate::handlers::runtime_hosts_workflow_api_tests::post_json(
            &app,
            "/api/runtime-hosts/host-a/runtime-jobs/claim".to_string(),
            json!({"lease_secs": 60}),
        )
        .await?;
        let original_expires_at: DateTime<Utc> =
            serde_json::from_value(claimed["lease_expires_at"].clone())?;
        let lease_generation = claimed["lease_generation"]
            .as_u64()
            .ok_or_else(|| anyhow::anyhow!("claimed lease generation must be an integer"))?;
        let lease_proof: Uuid = serde_json::from_value(claimed["lease_proof"].clone())?;
        let reserved = replay_completion_reservation(
            store.as_ref(),
            &job.id,
            "host-a",
            lease_generation,
            original_expires_at,
            Some(lease_proof),
        )
        .await?;
        let RuntimeJobLeaseRenewalOutcome::Renewed {
            lease_expires_at: reserved_expires_at,
            ..
        } = reserved
        else {
            anyhow::bail!("completion reservation should renew the lease");
        };
        let reconciled = crate::handlers::runtime_hosts_workflow_api_tests::post_json(
            &app,
            format!(
                "/api/runtime-hosts/host-a/runtime-jobs/{}/lease/renew",
                job.id
            ),
            json!({
                "lease_generation": lease_generation,
                "lease_expires_at": original_expires_at,
                "lease_proof": lease_proof,
                "renewal_id": Uuid::new_v4(),
                "lease_secs": 120,
            }),
        )
        .await?;
        assert_eq!(reconciled["renewed"], true);
        assert_eq!(reconciled["replayed"], true);
        assert_eq!(reconciled["lease_expires_at"], json!(reserved_expires_at));
        crate::handlers::runtime_hosts_workflow_api_tests::post_json(
            &app,
            "/api/runtime-hosts/host-a/deregister".to_string(),
            json!({}),
        )
        .await?;
        let stale_completion = store
            .commit_runtime_activity_completion_if_owned_with_generation(
                &job.id,
                RuntimeJobCompletionLease::remote(
                    "host-a",
                    reserved_expires_at,
                    lease_generation,
                    None,
                ),
                &ActivityResult::succeeded("remote_check", "done"),
            )
            .await?;
        assert!(
            stale_completion.is_none(),
            "deregistration must revoke the reserved lease before final completion"
        );
        Ok(())
    }
}
