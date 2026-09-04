#[tokio::test]
async fn runtime_job_claim_requires_lease_proof_capability_before_mutating_job(
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let Some((_state, store)) = make_test_state_with_runtime_store(dir.path()).await? else {
        return Ok(());
    };
    let app = runtime_hosts_workflow_app(_state);
    register_host_with_exact_capabilities(&app, "legacy-host", Vec::new()).await?;
    let job = enqueue_runtime_host_test_job(
        &store,
        "lease-proof-capability",
        RuntimeKind::RemoteHost,
        "remote-host-default",
        json!({ "activity": "remote_check" }),
    )
    .await?;

    let (status, legacy_claim) = post_json_with_status(
        &app,
        "/api/runtime-hosts/legacy-host/runtime-jobs/claim".to_string(),
        json!({}),
    )
    .await?;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(legacy_claim["claimed"], false);
    assert_eq!(legacy_claim["upgrade_required"], true);
    assert_eq!(
        legacy_claim["required_capability"],
        RUNTIME_JOB_LEASE_PROOF_V1_CAPABILITY
    );
    let pending = store
        .get_runtime_job(&job.id)
        .await?
        .expect("capability-rejected job should remain readable");
    assert_eq!(pending.status, RuntimeJobStatus::Pending);
    assert!(pending.lease.is_none());
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM runtime_job_lease_issuances WHERE runtime_job_id = $1",
        )
        .bind(&job.id)
        .fetch_one(store.pool())
        .await?,
        0
    );

    register_host(&app, "upgraded-host").await?;
    let upgraded_claim = post_json(
        &app,
        "/api/runtime-hosts/upgraded-host/runtime-jobs/claim".to_string(),
        json!({}),
    )
    .await?;
    assert_eq!(upgraded_claim["claimed"], true);
    assert!(upgraded_claim["lease_proof"].as_str().is_some());
    Ok(())
}

#[tokio::test]
async fn runtime_job_claim_endpoint_claims_remote_host_jobs_only() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let Some((state, store)) = make_test_state_with_runtime_store(dir.path()).await? else {
        return Ok(());
    };
    let app = runtime_hosts_workflow_app(state);
    register_host(&app, "host-a").await?;

    let local_job = enqueue_runtime_host_test_job(
        &store,
        "command-local",
        RuntimeKind::CodexJsonrpc,
        "codex-default",
        json!({ "activity": "local_check" }),
    )
    .await?;
    let remote_job = enqueue_runtime_host_test_job(
        &store,
        "command-remote",
        RuntimeKind::RemoteHost,
        "remote-host-default",
        json!({
            "activity": "remote_check",
            "workflow_id": "wf-remote",
            "runtime_profile": {
                "name": "remote-host-default",
                "kind": "remote_host"
            }
        }),
    )
    .await?;

    let json = post_json(
        &app,
        "/api/runtime-hosts/host-a/runtime-jobs/claim".to_string(),
        json!({ "lease_secs": 60 }),
    )
    .await?;
    assert_eq!(json["claimed"], true);
    assert_eq!(json["runtime_job_id"], remote_job.id);
    assert_eq!(json["lease_generation"], 1);
    assert!(json["lease_proof"].as_str().is_some());
    assert_eq!(json["runtime_job"]["runtime_kind"], "remote_host");
    assert_eq!(json["runtime_job"]["input"]["activity"], "remote_check");
    assert_eq!(
        json["runtime_job"]["input"]["runtime_profile"]["name"],
        "remote-host-default"
    );

    let local = store
        .get_runtime_job(&local_job.id)
        .await?
        .expect("local job should remain pending");
    assert_eq!(local.status, RuntimeJobStatus::Pending);
    Ok(())
}

#[tokio::test]
async fn runtime_job_claim_endpoint_includes_eval_credential_environment_policy(
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let Some((state, store)) = make_test_state_with_runtime_store(dir.path()).await? else {
        return Ok(());
    };
    let app = runtime_hosts_workflow_app(state);
    register_host_with_capabilities(&app, "host-a", vec!["eval_resource_limits"]).await?;

    let job = enqueue_runtime_host_test_job(
        &store,
        "command-eval-credential-policy",
        RuntimeKind::RemoteHost,
        "remote-host-default",
        json!({
            "activity": "implement_issue",
            "workflow_id": "wf-eval",
            "runtime_profile": {
                "name": "remote-host-default",
                "kind": "remote_host"
            },
            "command": {
                "eval": {
                    "eval_run_id": "run-1",
                    "timeout_secs": 45,
                    "plain_env_allowlist": [
                        "SAFE_FLAG",
                        "GITHUB_TOKEN",
                        "AWS_SECRET_ACCESS_KEY"
                    ]
                }
            }
        }),
    )
    .await?;

    let json = post_json(
        &app,
        "/api/runtime-hosts/host-a/runtime-jobs/claim".to_string(),
        json!({ "lease_secs": 60 }),
    )
    .await?;

    assert_eq!(json["claimed"], true);
    assert_eq!(json["runtime_job_id"], job.id);
    let policy = &json["runtime_job"]["input"]["command"]["eval"]["credential_environment"];
    assert_eq!(
        policy["schema"],
        crate::eval_credentials::EVAL_CREDENTIAL_ENVIRONMENT_SCHEMA_VERSION
    );
    assert_eq!(policy["secret_inheritance"], "empty_by_default");
    assert_eq!(policy["plain_env_allowlist"][0], "AWS_SECRET_ACCESS_KEY");
    assert_eq!(policy["plain_env_allowlist"][1], "GITHUB_TOKEN");
    assert_eq!(policy["plain_env_allowlist"][2], "SAFE_FLAG");
    assert_eq!(policy["plain_env_keys"].as_array().map(Vec::len), Some(0));
    assert_eq!(
        policy["credential_grants"].as_array().map(Vec::len),
        Some(0)
    );
    assert_eq!(policy["stripped_env"][0]["key"], "AWS_SECRET_ACCESS_KEY");
    assert_eq!(policy["stripped_env"][0]["class"], "cloud");
    assert_eq!(policy["stripped_env"][1]["key"], "GITHUB_TOKEN");
    assert_eq!(policy["stripped_env"][1]["class"], "github");
    assert_eq!(json["credential_environment"], *policy);
    assert_eq!(
        json["credential_environment_variables"]
            .as_object()
            .map(serde_json::Map::len),
        Some(0)
    );
    let events = store
        .events_for("runtime-host-test-command-eval-credential-policy")
        .await?;
    assert!(events.iter().any(|event| {
        event.event_type == "RuntimeHostEvalCredentialPolicyIssued"
            && event.event["runtime_job_id"] == job.id
            && event.event["credential_environment"] == *policy
    }));
    Ok(())
}

#[tokio::test]
async fn trusted_eval_job_is_claimed_only_by_a_capable_runtime_host() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let Some((state, store)) = make_test_state_with_runtime_store(dir.path()).await? else {
        return Ok(());
    };
    let app = runtime_hosts_workflow_app(state);
    register_host_with_capabilities(&app, "host-limited", vec!["eval_resource_limits"]).await?;
    register_host_with_capabilities(
        &app,
        "host-trusted",
        vec!["eval_resource_limits", "trusted_eval_verifier_v1"],
    )
    .await?;
    let job = enqueue_runtime_host_test_job(
        &store,
        "command-trusted-eval",
        RuntimeKind::RemoteHost,
        "eval-isolated-runtime-host",
        json!({
            "activity": "run_quality_gate",
            "command": {
                "eval": {
                    "eval_run_id": "run-1",
                    "case_id": "gh1454-scoped-ci-jobs",
                    "timeout_secs": 45,
                    "required_runtime_host_capabilities": [
                        "eval_resource_limits",
                        "trusted_eval_verifier_v1"
                    ]
                }
            }
        }),
    )
    .await?;

    let limited = post_json(
        &app,
        "/api/runtime-hosts/host-limited/runtime-jobs/claim".to_string(),
        json!({ "lease_secs": 60 }),
    )
    .await?;
    assert_eq!(limited["claimed"], false);

    let trusted = post_json(
        &app,
        "/api/runtime-hosts/host-trusted/runtime-jobs/claim".to_string(),
        json!({ "lease_secs": 60 }),
    )
    .await?;
    assert_eq!(trusted["claimed"], true);
    assert_eq!(trusted["runtime_job_id"], job.id);
    Ok(())
}

#[tokio::test]
async fn runtime_job_claim_endpoint_hydrates_verified_exact_replay_transcript() -> anyhow::Result<()>
{
    let dir = tempfile::tempdir()?;
    let Some((state, store)) = make_test_state_with_runtime_store(dir.path()).await? else {
        return Ok(());
    };
    let app = runtime_hosts_workflow_app(state);
    register_host(&app, "host-a").await?;

    let producer = enqueue_runtime_host_test_job(
        &store,
        "replay-producer",
        RuntimeKind::CodexExec,
        "codex-default",
        json!({ "activity": "implement_issue" }),
    )
    .await?;
    let content = "verified provider transcript";
    let reconstructed = store
        .reconstruct_runtime_transcript(
            "runtime-host-test-replay-producer",
            &producer.id,
            content,
            None,
            "test",
        )
        .await?;
    let consumer = enqueue_runtime_host_test_job(
        &store,
        "replay-consumer",
        RuntimeKind::RemoteHost,
        "remote-host-default",
        json!({
            "activity": "exact_replay",
            "command": {
                "activity": "exact_replay",
                "exact_replay": {
                    "transcript_artifact_ref": reconstructed.reference.artifact_ref,
                },
            },
        }),
    )
    .await?;

    let claimed = post_json(
        &app,
        "/api/runtime-hosts/host-a/runtime-jobs/claim".to_string(),
        json!({ "lease_secs": 60 }),
    )
    .await?;

    assert_eq!(claimed["claimed"], true);
    assert_eq!(claimed["runtime_job_id"], consumer.id);
    assert_eq!(
        claimed["runtime_job"]["input"]["command"]["exact_replay"]["transcript"],
        content
    );
    assert_eq!(
        claimed["runtime_job"]["input"]["command"]["exact_replay"]["verified_transcript"]
            ["checksum"],
        reconstructed.reference.checksum
    );
    Ok(())
}

#[tokio::test]
async fn runtime_job_claim_endpoint_completes_missing_exact_replay_before_dispatch(
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let Some((state, store)) = make_test_state_with_runtime_store(dir.path()).await? else {
        return Ok(());
    };
    let app = runtime_hosts_workflow_app(state.clone());
    register_host(&app, "host-a").await?;
    let job = enqueue_runtime_host_test_job(
        &store,
        "missing-replay",
        RuntimeKind::RemoteHost,
        "remote-host-default",
        json!({
            "activity": "exact_replay",
            "command": {
                "activity": "exact_replay",
                "exact_replay": {
                    "transcript_artifact_ref": "runtime-transcript:missing",
                },
            },
        }),
    )
    .await?;

    let claimed = post_json(
        &app,
        "/api/runtime-hosts/host-a/runtime-jobs/claim".to_string(),
        json!({ "lease_secs": 60 }),
    )
    .await?;

    assert_eq!(claimed["claimed"], false);
    assert_eq!(claimed["preflight_failed"], true);
    assert_eq!(claimed["runtime_job_id"], job.id);
    assert_eq!(claimed["runtime_job"]["status"], "failed");
    assert_eq!(
        claimed["runtime_job"]["output"]["signals"][0]["signal"]["stop_reason_code"],
        "runtime_transcript_lost"
    );
    let persisted = store
        .get_runtime_job(&job.id)
        .await?
        .expect("preflight-failed job should remain auditable");
    assert_eq!(persisted.status, RuntimeJobStatus::Failed);
    assert!(persisted.lease.is_none());
    assert!(
        state
            .runtime_circuit_breakers
            .snapshots(Utc::now())
            .is_empty(),
        "transcript preflight failures must not count against the agent runtime"
    );
    Ok(())
}

#[tokio::test]
async fn runtime_job_claim_endpoint_blocks_duplicate_claims() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let Some((state, store)) = make_test_state_with_runtime_store(dir.path()).await? else {
        return Ok(());
    };
    let app = runtime_hosts_workflow_app(state);
    register_host(&app, "host-a").await?;
    register_host(&app, "host-b").await?;

    let job = enqueue_runtime_host_test_job(
        &store,
        "command-remote",
        RuntimeKind::RemoteHost,
        "remote-host-default",
        json!({ "activity": "remote_check" }),
    )
    .await?;
    let first = post_json(
        &app,
        "/api/runtime-hosts/host-a/runtime-jobs/claim".to_string(),
        json!({ "lease_secs": 60 }),
    )
    .await?;
    assert_eq!(first["runtime_job_id"], job.id);

    let second = post_json(
        &app,
        "/api/runtime-hosts/host-b/runtime-jobs/claim".to_string(),
        json!({ "lease_secs": 60 }),
    )
    .await?;
    assert_eq!(second["claimed"], false);
    Ok(())
}

#[tokio::test]
async fn runtime_job_claim_endpoint_defers_open_circuit_profile() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let Some((state, store)) = make_test_state_with_runtime_store(dir.path()).await? else {
        return Ok(());
    };
    let app = runtime_hosts_workflow_app(state.clone());
    register_host(&app, "host-a").await?;

    let job = enqueue_runtime_host_test_job(
        &store,
        "command-open-circuit",
        RuntimeKind::RemoteHost,
        "remote-host-default",
        json!({ "activity": "remote_check" }),
    )
    .await?;
    let now = Utc::now();
    for index in 0..5 {
        state.runtime_circuit_breakers.record_failure(
            "remote-host-default",
            &format!("seed-failure-{index}"),
            crate::runtime_circuit_breaker::FailureClass::QuotaInteractiveWait,
            now,
        );
    }

    let json = post_json(
        &app,
        "/api/runtime-hosts/host-a/runtime-jobs/claim".to_string(),
        json!({ "lease_secs": 60 }),
    )
    .await?;

    assert_eq!(json["claimed"], false);
    let deferred = store
        .get_runtime_job(&job.id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("remote host job should still exist"))?;
    assert_eq!(deferred.status, RuntimeJobStatus::Pending);
    assert!(deferred.lease.is_none());
    assert!(deferred
        .not_before
        .is_some_and(|not_before| not_before > now));
    let events = store.runtime_events_for(&job.id).await?;
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].event_type, "RuntimeJobClaimed");
    assert_eq!(events[1].event_type, "RuntimeJobClaimDeferred");
    assert_eq!(events[1].event["claim_api"], "runtime_host");
    Ok(())
}

#[tokio::test]
async fn runtime_job_claim_endpoint_skips_eval_job_without_resource_limit_capability(
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let Some((state, store)) = make_test_state_with_runtime_store(dir.path()).await? else {
        return Ok(());
    };
    let app = runtime_hosts_workflow_app(state);
    register_host(&app, "host-a").await?;

    let job = enqueue_runtime_host_test_job(
        &store,
        "eval-missing-capability",
        RuntimeKind::RemoteHost,
        "remote-host-default",
        json!({
            "activity": "implement_issue",
            "command": {
                "activity": "implement_issue",
                "eval": {
                    "eval_run_id": "run-1",
                    "case_id": "case-1",
                    "timeout_secs": 45
                }
            }
        }),
    )
    .await?;
    let json = post_json(
        &app,
        "/api/runtime-hosts/host-a/runtime-jobs/claim".to_string(),
        json!({ "lease_secs": 60 }),
    )
    .await?;

    assert_eq!(json["claimed"], false);
    let pending = store
        .get_runtime_job(&job.id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("eval job should still exist"))?;
    assert_eq!(pending.status, RuntimeJobStatus::Pending);
    assert!(pending.lease.is_none());
    assert!(pending.not_before.is_none());
    let events = store.runtime_events_for(&job.id).await?;
    assert!(events.is_empty());
    Ok(())
}

#[tokio::test]
async fn runtime_job_claim_endpoint_reclaims_expired_remote_job() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let Some((state, store)) = make_test_state_with_runtime_store(dir.path()).await? else {
        return Ok(());
    };
    let app = runtime_hosts_workflow_app(state);
    register_host(&app, "host-a").await?;
    register_host(&app, "host-b").await?;

    let job = enqueue_runtime_host_test_job(
        &store,
        "command-remote",
        RuntimeKind::RemoteHost,
        "remote-host-default",
        json!({ "activity": "remote_check" }),
    )
    .await?;
    let first = store
        .claim_next_runtime_job_for_runtime_kind(
            RuntimeKind::RemoteHost,
            "host-a",
            Utc::now() - chrono::TimeDelta::seconds(1),
        )
        .await?
        .ok_or_else(|| anyhow::anyhow!("host-a should claim the runtime job"))?;
    assert_eq!(first.id, job.id);

    let second = post_json(
        &app,
        "/api/runtime-hosts/host-b/runtime-jobs/claim".to_string(),
        json!({ "lease_secs": 60 }),
    )
    .await?;
    assert_eq!(second["runtime_job_id"], job.id);
    let persisted = store
        .get_runtime_job(&job.id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("runtime job should still exist"))?;
    assert_eq!(
        persisted
            .lease
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("runtime job should be leased"))?
            .owner,
        "host-b"
    );
    Ok(())
}
