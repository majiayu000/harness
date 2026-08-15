use super::*;
use harness_workflow::runtime::store::runtime_job_leases::RuntimeJobLeaseRenewalOutcome;
use uuid::Uuid;

#[test]
fn eval_resource_limits_derive_from_timeout_when_missing() {
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

    let limits = eval_resource_limit_enforcement_for_job(&job)
        .expect("limits should parse")
        .expect("eval job should require limits");

    assert_eq!(limits.effective.wall_time_secs, Some(45));
    assert_eq!(limits.effective.cpu_time_secs, Some(45));
    assert_eq!(limits.effective.output_bytes, Some(64 * 1024 * 1024));
}

#[test]
fn eval_resource_limit_report_is_required_for_eval_completion() {
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

    let err = completion::validate_eval_resource_limit_report(&job, &result)
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
                    "timeout_secs": 45,
                    "resource_limits": limits.clone()
                }
            }
        }),
    );
    let result =
        ActivityResult::succeeded("implement_issue", "done").with_artifact(ActivityArtifact::new(
            "resource_limit_report",
            json!(ResourceLimitReport {
                limits,
                usage: harness_sandbox::ResourceUsage {
                    output_bytes: Some(128),
                    wall_time_millis: Some(1000),
                    ..Default::default()
                },
                termination: None,
                reason: "completed within resource limits".to_string(),
            }),
        ));

    completion::validate_eval_resource_limit_report(&job, &result)
        .expect("matching resource report should be accepted");
}

#[test]
fn eval_resource_limit_report_requires_usage_evidence_and_reason() {
    let limits = ResourceLimits::evaluation_defaults(45)
        .cap_by(ResourceLimits::operator_default_maxima())
        .expect("limits should cap");
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
                    "timeout_secs": 45,
                    "resource_limits": limits.clone()
                }
            }
        }),
    );
    let empty_usage =
        ActivityResult::succeeded("implement_issue", "done").with_artifact(ActivityArtifact::new(
            "resource_limit_report",
            json!(ResourceLimitReport {
                limits: limits.clone(),
                usage: harness_sandbox::ResourceUsage::default(),
                termination: None,
                reason: "completed within resource limits".to_string(),
            }),
        ));

    let err = completion::validate_eval_resource_limit_report(&job, &empty_usage)
        .expect_err("empty usage should fail closed");
    assert_eq!(err.0, StatusCode::BAD_REQUEST);
    assert_eq!(
        err.1["error"],
        "resource_limit_report requires usage evidence"
    );

    let empty_reason =
        ActivityResult::succeeded("implement_issue", "done").with_artifact(ActivityArtifact::new(
            "resource_limit_report",
            json!(ResourceLimitReport {
                limits,
                usage: harness_sandbox::ResourceUsage {
                    wall_time_millis: Some(1000),
                    ..Default::default()
                },
                termination: None,
                reason: " ".to_string(),
            }),
        ));
    let err = completion::validate_eval_resource_limit_report(&job, &empty_reason)
        .expect_err("empty reason should fail closed");
    assert_eq!(err.0, StatusCode::BAD_REQUEST);
    assert_eq!(
        err.1["error"],
        "resource_limit_report requires a non-empty reason"
    );
}

#[test]
fn post_reservation_errors_return_the_updated_lease_fence() {
    let lease_expires_at = Utc::now() + chrono::TimeDelta::minutes(5);
    let response =
        completion::reserved_lease_error_response("verification failed", lease_expires_at, 7);

    assert_eq!(response.0["error"], "verification failed");
    assert_eq!(response.0["lease_expires_at"], json!(lease_expires_at));
    assert_eq!(response.0["lease_generation"], 7);
    assert_eq!(response.0["lease_reserved"], true);
}

#[test]
fn completion_reservation_id_is_stable_for_ambiguous_retries() {
    let lease_expires_at = Utc::now() + chrono::TimeDelta::minutes(5);
    let first = completion::completion_reservation_id("job-1", "host-a", 3, lease_expires_at);
    let retry = completion::completion_reservation_id("job-1", "host-a", 3, lease_expires_at);
    let different_owner =
        completion::completion_reservation_id("job-1", "host-b", 3, lease_expires_at);

    assert_eq!(first, retry);
    assert_ne!(first, different_owner);
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
    let app = crate::handlers::runtime_hosts_workflow_api_tests::runtime_hosts_workflow_app(state);
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

    let reserved = completion::replay_completion_reservation(
        store.as_ref(),
        &job.id,
        "host-a",
        lease_generation,
        original_expires_at,
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
            "host-a",
            reserved_expires_at,
            Some(lease_generation),
            &ActivityResult::succeeded("remote_check", "done"),
        )
        .await?;
    assert!(
        stale_completion.is_none(),
        "deregistration must revoke the reserved lease before final completion"
    );
    Ok(())
}

#[tokio::test]
async fn register_runtime_host_rejects_required_missing_runtime_state_store() -> anyhow::Result<()>
{
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let mut state = Arc::new(crate::test_helpers::make_test_state(dir.path()).await?);
    let state_mut =
        Arc::get_mut(&mut state).ok_or_else(|| anyhow::anyhow!("expected unique state"))?;
    state_mut.startup_statuses =
        vec![
            crate::http::state::StoreStartupResult::optional("runtime_state_store")
                .failed("pool timed out while waiting for an open connection"),
        ];
    state_mut.degraded_subsystems = vec!["runtime_state_store"];

    let (status, body) = register_runtime_host(
        State(state.clone()),
        Json(RegisterRuntimeHostRequest {
            host_id: "host-a".to_string(),
            display_name: None,
            capabilities: vec![],
        }),
    )
    .await;

    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body.0["error"], "runtime state persistence unavailable");
    assert!(
        !state.runtime_hosts.hosts.contains_key("host-a"),
        "host registration must not mutate memory when required persistence is unavailable"
    );
    assert!(state.is_runtime_state_dirty());
    Ok(())
}

#[tokio::test]
async fn deregister_runtime_host_rejects_required_missing_runtime_state_store_before_lookup(
) -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let mut state = Arc::new(crate::test_helpers::make_test_state(dir.path()).await?);
    let state_mut =
        Arc::get_mut(&mut state).ok_or_else(|| anyhow::anyhow!("expected unique state"))?;
    state_mut.startup_statuses =
        vec![
            crate::http::state::StoreStartupResult::optional("runtime_state_store")
                .failed("pool timed out while waiting for an open connection"),
        ];
    state_mut.degraded_subsystems = vec!["runtime_state_store"];

    let (status, body) =
        deregister_runtime_host(State(state.clone()), Path("ghost-host".to_string())).await;

    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body.0["error"], "runtime state persistence unavailable");
    assert!(state.is_runtime_state_dirty());
    Ok(())
}
