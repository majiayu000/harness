async fn assert_stale_completion_dead_lettered(
    app: &Router,
    store: &WorkflowRuntimeStore,
    job: &RuntimeJob,
    lease_generation: u64,
    lease_expires_at: chrono::DateTime<Utc>,
    lease_proof: uuid::Uuid,
) -> anyhow::Result<()> {
    let uri = format!("/api/runtime-hosts/host-a/runtime-jobs/{}/complete", job.id);
    let request = json!({
        "lease_generation": lease_generation,
        "lease_expires_at": lease_expires_at,
        "lease_proof": lease_proof,
        "result": ActivityResult::succeeded("remote_check", "stale result"),
    });
    let (status, body) = post_json_with_status(app, uri.clone(), request.clone()).await?;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["completed"], false);
    assert_eq!(body["dead_lettered"], true);
    let (replay_status, replay) = post_json_with_status(app, uri, request).await?;
    assert_eq!(replay_status, StatusCode::CONFLICT);
    assert_eq!(replay["completed"], false);
    assert_eq!(
        replay["dead_lettered"], true,
        "an exact response-loss replay must report the existing dead letter"
    );
    let (persisted_generation,): (Option<i64>,) = sqlx::query_as(
        "SELECT lease_generation FROM runtime_job_completions_dlq WHERE runtime_job_id = $1",
    )
    .bind(&job.id)
    .fetch_one(store.pool())
    .await?;
    assert_eq!(persisted_generation, Some(lease_generation as i64));
    let events = store.runtime_events_for(&job.id).await?;
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event_type == "LeaseExpiredCompletionRecorded")
            .count(),
        1,
        "an exact response-loss replay must not duplicate the audit event"
    );
    Ok(())
}

#[tokio::test]
async fn runtime_job_completion_endpoint_accepts_terminal_activity_result() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let Some((state, store)) = make_test_state_with_runtime_store(dir.path()).await? else {
        return Ok(());
    };
    let app = runtime_hosts_workflow_app(state);
    register_host(&app, "host-a").await?;

    let job = enqueue_runtime_host_test_job(
        &store,
        "command-remote",
        RuntimeKind::RemoteHost,
        "remote-host-default",
        json!({ "activity": "remote_check" }),
    )
    .await?;
    let claimed = post_json(
        &app,
        "/api/runtime-hosts/host-a/runtime-jobs/claim".to_string(),
        json!({ "lease_secs": 60 }),
    )
    .await?;
    let lease_expires_at = claimed["lease_expires_at"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("lease_expires_at must be a string"))?;

    let mut result = ActivityResult::failed(
        "remote_check",
        "Remote host reported a failed activity.",
        "remote execution failed",
    )
    .with_signal(ActivitySignal::new(
        "RuntimeTranscriptUnavailable",
        json!({"stop_reason_code": "runtime_transcript_lost"}),
    ));
    for artifact_type in
        harness_workflow::runtime::completion_evidence::SERVER_RESERVED_ARTIFACT_TYPES
    {
        result = result.with_artifact(ActivityArtifact::new(
            artifact_type,
            json!({"forged": true}),
        ));
    }
    result = result.with_artifact(ActivityArtifact::new(
        "remote_diagnostic",
        json!({"kept": true}),
    ));
    let completed = post_json(
        &app,
        format!("/api/runtime-hosts/host-a/runtime-jobs/{}/complete", job.id),
        json!({
            "lease_generation": claimed["lease_generation"],
            "lease_expires_at": lease_expires_at,
            "lease_proof": claimed["lease_proof"],
            "result": result,
        }),
    )
    .await?;
    assert_eq!(completed["completed"], true);
    assert_eq!(completed["runtime_job"]["status"], "failed");
    assert_eq!(completed["runtime_job"]["error"], "remote execution failed");
    assert_eq!(completed["runtime_job"]["output"]["signals"], json!([]));
    let expected_artifacts = json!([{
        "artifact_type": "remote_diagnostic",
        "artifact": {"kept": true},
    }]);
    assert_eq!(
        completed["runtime_job"]["output"]["artifacts"],
        expected_artifacts
    );

    let persisted = store
        .get_runtime_job(&job.id)
        .await?
        .expect("runtime job should be persisted");
    assert_eq!(persisted.status, RuntimeJobStatus::Failed);
    assert!(persisted.lease.is_none());
    let persisted_output = persisted.output.expect("completed job output");
    assert_eq!(persisted_output["signals"], json!([]));
    assert_eq!(persisted_output["artifacts"], expected_artifacts);

    let events = store.runtime_events_for(&job.id).await?;
    let result_event = events
        .iter()
        .find(|event| event.event_type == "ActivityResultReady")
        .expect("activity result event");
    assert_eq!(result_event.event["signals"], json!([]));
    assert_eq!(result_event.event["artifacts"], expected_artifacts);
    Ok(())
}

#[tokio::test]
async fn runtime_job_completion_endpoint_allows_draining_host_to_finish() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let Some((state, store)) = make_test_state_with_runtime_store(dir.path()).await? else {
        return Ok(());
    };
    let app = runtime_hosts_workflow_app(state.clone());
    register_host(&app, "host-a").await?;
    let job = enqueue_runtime_host_test_job(
        &store,
        "completion-while-draining",
        RuntimeKind::RemoteHost,
        "remote-host-default",
        json!({ "activity": "remote_check" }),
    )
    .await?;
    let claimed = post_json(
        &app,
        "/api/runtime-hosts/host-a/runtime-jobs/claim".to_string(),
        json!({ "lease_secs": 60 }),
    )
    .await?;
    assert!(state.runtime_hosts.mark_draining("host-a").is_some());

    let completed = post_json(
        &app,
        format!("/api/runtime-hosts/host-a/runtime-jobs/{}/complete", job.id),
        json!({
            "lease_generation": claimed["lease_generation"],
            "lease_expires_at": claimed["lease_expires_at"],
            "lease_proof": claimed["lease_proof"],
            "result": ActivityResult::succeeded("remote_check", "finished while draining"),
        }),
    )
    .await?;

    assert_eq!(completed["completed"], true);
    assert_eq!(completed["runtime_job"]["status"], "succeeded");
    Ok(())
}

#[tokio::test]
async fn draining_host_completion_revalidates_required_eval_capabilities() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let Some((state, store)) = make_test_state_with_runtime_store(dir.path()).await? else {
        return Ok(());
    };
    let app = runtime_hosts_workflow_app(state.clone());
    register_host_with_capabilities(
        &app,
        "host-a",
        vec!["eval_resource_limits", "trusted_eval_verifier_v1"],
    )
    .await?;
    let job = enqueue_runtime_host_test_job(
        &store,
        "draining-capability-revalidation",
        RuntimeKind::RemoteHost,
        "eval-isolated-runtime-host",
        json!({
            "activity": "run_quality_gate",
            "command": {
                "eval": {
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
    let claimed = post_json(
        &app,
        "/api/runtime-hosts/host-a/runtime-jobs/claim".to_string(),
        json!({ "lease_secs": 60 }),
    )
    .await?;
    assert_eq!(claimed["claimed"], true);
    assert_eq!(claimed["runtime_job_id"], job.id);
    assert!(state.runtime_hosts.mark_draining("host-a").is_some());
    state.runtime_hosts.register(
        "host-a".to_string(),
        None,
        vec![RUNTIME_JOB_LEASE_PROOF_V1_CAPABILITY.to_string()],
    );

    let (status, body) = post_json_with_status(
        &app,
        format!("/api/runtime-hosts/host-a/runtime-jobs/{}/complete", job.id),
        json!({
            "lease_generation": claimed["lease_generation"],
            "lease_expires_at": claimed["lease_expires_at"],
            "lease_proof": claimed["lease_proof"],
            "result": ActivityResult::succeeded("run_quality_gate", "untrusted result"),
        }),
    )
    .await?;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        body["error"],
        "runtime host no longer advertises required eval capabilities"
    );
    assert_eq!(
        body["missing_capabilities"],
        json!(["eval_resource_limits", "trusted_eval_verifier_v1"])
    );
    Ok(())
}

#[tokio::test]
async fn runtime_job_completion_endpoint_dead_letters_expired_issued_lease() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let Some((state, store)) = make_test_state_with_runtime_store(dir.path()).await? else {
        return Ok(());
    };
    let app = runtime_hosts_workflow_app(state);
    register_host(&app, "host-a").await?;
    let job = enqueue_runtime_host_test_job(
        &store,
        "completion-expired-issued-lease",
        RuntimeKind::RemoteHost,
        "remote-host-default",
        json!({ "activity": "remote_check" }),
    )
    .await?;
    let lease_expires_at = Utc::now() - chrono::TimeDelta::seconds(1);
    let claimed = store
        .claim_next_runtime_job_for_runtime_kind(
            RuntimeKind::RemoteHost,
            "host-a",
            lease_expires_at,
        )
        .await?
        .expect("remote job should be claimed");
    let lease_proof = store
        .remote_runtime_job_lease_proof(
            &job.id,
            "host-a",
            claimed.lease_generation,
            lease_expires_at,
        )
        .await?
        .expect("remote claim should have a lease proof");

    assert_stale_completion_dead_lettered(
        &app,
        &store,
        &job,
        claimed.lease_generation,
        lease_expires_at,
        lease_proof,
    )
    .await
}

#[tokio::test]
async fn runtime_job_completion_endpoint_dead_letters_reclaimed_issued_lease() -> anyhow::Result<()>
{
    let dir = tempfile::tempdir()?;
    let Some((state, store)) = make_test_state_with_runtime_store(dir.path()).await? else {
        return Ok(());
    };
    let app = runtime_hosts_workflow_app(state);
    register_host(&app, "host-a").await?;
    let job = enqueue_runtime_host_test_job(
        &store,
        "completion-reclaimed-issued-lease",
        RuntimeKind::RemoteHost,
        "remote-host-default",
        json!({ "activity": "remote_check" }),
    )
    .await?;
    let lease_expires_at = Utc::now() - chrono::TimeDelta::seconds(1);
    let first = store
        .claim_next_runtime_job_for_runtime_kind(
            RuntimeKind::RemoteHost,
            "host-a",
            lease_expires_at,
        )
        .await?
        .expect("remote job should be claimed");
    let lease_proof = store
        .remote_runtime_job_lease_proof(&job.id, "host-a", first.lease_generation, lease_expires_at)
        .await?
        .expect("remote claim should have a lease proof");
    let reclaimed = store
        .claim_next_runtime_job_for_runtime_kind(
            RuntimeKind::RemoteHost,
            "host-b",
            Utc::now() + chrono::TimeDelta::minutes(5),
        )
        .await?
        .expect("expired lease should be reclaimed");
    assert!(reclaimed.lease_generation > first.lease_generation);

    assert_stale_completion_dead_lettered(
        &app,
        &store,
        &job,
        first.lease_generation,
        lease_expires_at,
        lease_proof,
    )
    .await
}

#[tokio::test]
async fn legacy_generation_omitting_completion_is_dead_lettered_after_reclaim() -> anyhow::Result<()>
{
    let dir = tempfile::tempdir()?;
    let Some((state, store)) = make_test_state_with_runtime_store(dir.path()).await? else {
        return Ok(());
    };
    let app = runtime_hosts_workflow_app(state);
    register_host(&app, "host-a").await?;
    let job = enqueue_runtime_host_test_job(
        &store,
        "completion-reclaimed-legacy-lease",
        RuntimeKind::RemoteHost,
        "remote-host-default",
        json!({ "activity": "remote_check" }),
    )
    .await?;
    let legacy_expires_at = Utc::now() - chrono::TimeDelta::seconds(1);
    let legacy = store
        .claim_next_runtime_job_for_runtime_kind(
            RuntimeKind::RemoteHost,
            "host-a",
            legacy_expires_at,
        )
        .await?
        .expect("legacy job should be claimed");
    sqlx::query(
        "UPDATE runtime_job_lease_issuances
         SET lease_proof = NULL, legacy_proofless = TRUE
         WHERE runtime_job_id = $1
           AND owner = 'host-a'
           AND lease_generation = $2
           AND lease_expires_at = $3",
    )
    .bind(&job.id)
    .bind(i64::try_from(legacy.lease_generation)?)
    .bind(legacy_expires_at)
    .execute(store.pool())
    .await?;
    let reclaimed = store
        .claim_next_runtime_job_for_runtime_kind(
            RuntimeKind::RemoteHost,
            "host-b",
            Utc::now() + chrono::TimeDelta::minutes(5),
        )
        .await?
        .expect("expired legacy lease should be reclaimed");
    assert!(reclaimed.lease_generation > legacy.lease_generation);

    let (status, body) = post_json_with_status(
        &app,
        format!("/api/runtime-hosts/host-a/runtime-jobs/{}/complete", job.id),
        json!({
            "lease_expires_at": legacy_expires_at,
            "result": ActivityResult::succeeded("remote_check", "legacy stale result"),
        }),
    )
    .await?;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(body["completed"], false);
    assert_eq!(body["dead_lettered"], true);
    let (generation,): (Option<i64>,) = sqlx::query_as(
        "SELECT lease_generation
         FROM runtime_job_completions_dlq
         WHERE runtime_job_id = $1",
    )
    .bind(&job.id)
    .fetch_one(store.pool())
    .await?;
    assert_eq!(generation, Some(i64::try_from(legacy.lease_generation)?));
    Ok(())
}

#[tokio::test]
async fn legacy_generation_omitting_completion_replays_rotated_reservation() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let Some((state, store)) = make_test_state_with_runtime_store(dir.path()).await? else {
        return Ok(());
    };
    let app = runtime_hosts_workflow_app(state);
    register_host(&app, "host-a").await?;
    let job = enqueue_runtime_host_test_job(
        &store,
        "completion-legacy-reservation-replay",
        RuntimeKind::RemoteHost,
        "remote-host-default",
        json!({ "activity": "remote_check" }),
    )
    .await?;
    let legacy_expires_at = Utc::now() + chrono::TimeDelta::minutes(5);
    let legacy = store
        .claim_next_runtime_job_for_runtime_kind(
            RuntimeKind::RemoteHost,
            "host-a",
            legacy_expires_at,
        )
        .await?
        .expect("legacy job should be claimed");
    sqlx::query(
        "UPDATE runtime_job_lease_issuances
         SET lease_proof = NULL, legacy_proofless = TRUE
         WHERE runtime_job_id = $1
           AND owner = 'host-a'
           AND lease_generation = $2
           AND lease_expires_at = $3",
    )
    .bind(&job.id)
    .bind(i64::try_from(legacy.lease_generation)?)
    .bind(legacy_expires_at)
    .execute(store.pool())
    .await?;
    assert!(matches!(
        runtime_hosts::replay_completion_reservation(
            store.as_ref(),
            &job.id,
            "host-a",
            legacy.lease_generation,
            legacy_expires_at,
            None,
        )
        .await?,
        RuntimeJobLeaseRenewalOutcome::Renewed {
            replayed: false,
            ..
        }
    ));
    assert!(store
        .remote_legacy_runtime_job_lease_generation(&job.id, "host-a", legacy_expires_at,)
        .await?
        .is_some());

    let completed = post_json(
        &app,
        format!("/api/runtime-hosts/host-a/runtime-jobs/{}/complete", job.id),
        json!({
            "lease_expires_at": legacy_expires_at,
            "result": ActivityResult::succeeded("remote_check", "legacy completion"),
        }),
    )
    .await?;
    assert_eq!(completed["completed"], true);
    assert_eq!(completed["runtime_job"]["status"], "succeeded");
    Ok(())
}

#[tokio::test]
async fn runtime_job_completion_endpoint_rejects_deregistered_host() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let Some((state, store)) = make_test_state_with_runtime_store(dir.path()).await? else {
        return Ok(());
    };
    let app = runtime_hosts_workflow_app(state);
    register_host(&app, "host-a").await?;
    let job = enqueue_runtime_host_test_job(
        &store,
        "completion-revoked-issued-lease",
        RuntimeKind::RemoteHost,
        "remote-host-default",
        json!({ "activity": "remote_check" }),
    )
    .await?;
    let claimed = post_json(
        &app,
        "/api/runtime-hosts/host-a/runtime-jobs/claim".to_string(),
        json!({ "lease_secs": 60 }),
    )
    .await?;
    let lease_generation = claimed["lease_generation"]
        .as_u64()
        .expect("claim should include lease generation");
    let lease_expires_at: chrono::DateTime<Utc> =
        serde_json::from_value(claimed["lease_expires_at"].clone())?;
    let lease_proof: uuid::Uuid = serde_json::from_value(claimed["lease_proof"].clone())?;
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/runtime-hosts/host-a/deregister")
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(response.status(), StatusCode::OK);

    let (status, body) = post_json_with_status(
        &app,
        format!("/api/runtime-hosts/host-a/runtime-jobs/{}/complete", job.id),
        json!({
            "lease_generation": lease_generation,
            "lease_expires_at": lease_expires_at,
            "lease_proof": lease_proof,
            "result": ActivityResult::succeeded("remote_check", "revoked result"),
        }),
    )
    .await?;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"], "runtime host 'host-a' is not registered");
    let (dead_letters,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM runtime_job_completions_dlq WHERE runtime_job_id = $1",
    )
    .bind(&job.id)
    .fetch_one(store.pool())
    .await?;
    assert_eq!(dead_letters, 0);
    Ok(())
}

#[tokio::test]
async fn runtime_job_completion_endpoint_rejects_unbound_remote_quality_gate_validation(
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let Some((state, store)) = make_test_state_with_runtime_store(dir.path()).await? else {
        return Ok(());
    };
    let app = runtime_hosts_workflow_app(state);
    register_host(&app, "host-a").await?;

    let job = enqueue_runtime_host_test_job(
        &store,
        "remote-quality-gate",
        RuntimeKind::RemoteHost,
        "remote-host-default",
        json!({
            "activity": harness_workflow::runtime::QUALITY_GATE_ACTIVITY,
            "validation_commands": ["true"],
        }),
    )
    .await?;
    let claimed = post_json(
        &app,
        "/api/runtime-hosts/host-a/runtime-jobs/claim".to_string(),
        json!({ "lease_secs": 60 }),
    )
    .await?;
    let result = ActivityResult::succeeded(
        harness_workflow::runtime::QUALITY_GATE_ACTIVITY,
        "Remote quality gate completed.",
    )
    .with_artifact(ActivityArtifact::new(
        harness_workflow::runtime::completion_evidence::ARTIFACT_SERVER_VALIDATION_DIGEST,
        json!({"forged": true}),
    ));

    let completed = post_json(
        &app,
        format!("/api/runtime-hosts/host-a/runtime-jobs/{}/complete", job.id),
        json!({
            "lease_generation": claimed["lease_generation"],
            "lease_expires_at": claimed["lease_expires_at"],
            "lease_proof": claimed["lease_proof"],
            "result": result,
        }),
    )
    .await?;

    assert_eq!(completed["runtime_job"]["output"]["status"], "failed");
    assert_eq!(
        completed["runtime_job"]["output"]["error_kind"],
        "configuration"
    );
    let artifacts = completed["runtime_job"]["output"]["artifacts"]
        .as_array()
        .expect("completion artifacts");
    assert!(artifacts.iter().all(|artifact| {
        artifact["artifact_type"]
            != harness_workflow::runtime::completion_evidence::ARTIFACT_SERVER_VALIDATION_DIGEST
    }));
    assert!(artifacts.iter().any(|artifact| {
        artifact["artifact_type"] == "remote_quality_gate_verification"
            && artifact["artifact"]["verified"] == false
    }));
    Ok(())
}

#[tokio::test]
async fn runtime_job_completion_preflight_error_preserves_the_client_lease_fence(
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let Some((state, store)) = make_test_state_with_runtime_store(dir.path()).await? else {
        return Ok(());
    };
    let app = runtime_hosts_workflow_app(state);
    register_host_with_capabilities(&app, "host-a", vec!["eval_resource_limits"]).await?;
    let job = enqueue_runtime_host_test_job(
        &store,
        "completion-preflight-fence",
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
    )
    .await?;
    let claimed = post_json(
        &app,
        "/api/runtime-hosts/host-a/runtime-jobs/claim".to_string(),
        json!({ "lease_secs": 60 }),
    )
    .await?;

    let (status, body) = post_json_with_status(
        &app,
        format!("/api/runtime-hosts/host-a/runtime-jobs/{}/complete", job.id),
        json!({
            "lease_generation": claimed["lease_generation"],
            "lease_expires_at": claimed["lease_expires_at"],
            "lease_proof": claimed["lease_proof"],
            "result": ActivityResult::succeeded("implement_issue", "done"),
        }),
    )
    .await?;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        body["error"],
        "eval runtime job completion requires resource_limit_report artifact"
    );
    assert!(body.get("lease_reserved").is_none());

    let renewed = post_json(
        &app,
        format!(
            "/api/runtime-hosts/host-a/runtime-jobs/{}/lease/renew",
            job.id
        ),
        json!({
            "lease_generation": claimed["lease_generation"],
            "lease_expires_at": claimed["lease_expires_at"],
            "lease_proof": claimed["lease_proof"],
            "renewal_id": uuid::Uuid::new_v4(),
            "lease_secs": 120,
        }),
    )
    .await?;
    assert_eq!(renewed["renewed"], true);
    Ok(())
}

#[tokio::test]
async fn runtime_job_completion_endpoint_persists_transcript_before_accepting_result(
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let Some((state, store)) = make_test_state_with_runtime_store(dir.path()).await? else {
        return Ok(());
    };
    let app = runtime_hosts_workflow_app(state);
    register_host(&app, "host-a").await?;

    let workflow_id = "runtime-host-test-transcript";
    let job = enqueue_runtime_host_test_job(
        &store,
        "transcript",
        RuntimeKind::RemoteHost,
        "remote-host-default",
        json!({
            "activity": "remote_check",
            "workflow_id": workflow_id,
        }),
    )
    .await?;
    let claimed = post_json(
        &app,
        "/api/runtime-hosts/host-a/runtime-jobs/claim".to_string(),
        json!({ "lease_secs": 60 }),
    )
    .await?;
    let lease_expires_at = claimed["lease_expires_at"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("lease_expires_at must be a string"))?;
    let result = ActivityResult::succeeded("remote_check", "Remote host completed the activity.")
        .with_artifact(ActivityArtifact::new(
            RUNTIME_TRANSCRIPT_SOURCE_ARTIFACT,
            json!({
                "content": "provider transcript bytes",
                "format": "provider.export.v1",
            }),
        ));

    let completed = post_json(
        &app,
        format!("/api/runtime-hosts/host-a/runtime-jobs/{}/complete", job.id),
        json!({
            "lease_generation": claimed["lease_generation"],
            "lease_expires_at": lease_expires_at,
            "lease_proof": claimed["lease_proof"],
            "result": result,
        }),
    )
    .await?;
    assert_eq!(completed["completed"], true);
    let artifacts = completed["runtime_job"]["output"]["artifacts"]
        .as_array()
        .expect("completed job artifacts");
    assert!(artifacts
        .iter()
        .any(|artifact| artifact["artifact_type"] == RUNTIME_TRANSCRIPT_ARTIFACT));
    assert!(artifacts
        .iter()
        .all(|artifact| artifact["artifact_type"] != RUNTIME_TRANSCRIPT_SOURCE_ARTIFACT));

    let artifact_ref = harness_workflow::runtime::runtime_transcript_artifact_ref(&job.id);
    match store.read_runtime_transcript(&artifact_ref).await? {
        RuntimeTranscriptRead::Verified(record) => {
            assert_eq!(record.workflow_id, workflow_id);
            assert_eq!(record.content, "provider transcript bytes");
        }
        other => anyhow::bail!("expected verified remote transcript, got {other:?}"),
    }
    let events = store.runtime_events_for(&job.id).await?;
    assert!(events.iter().all(|event| !event
        .event
        .to_string()
        .contains("provider transcript bytes")));
    Ok(())
}

#[tokio::test]
async fn runtime_job_completion_endpoint_returns_not_found_for_missing_job() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let Some((state, _store)) = make_test_state_with_runtime_store(dir.path()).await? else {
        return Ok(());
    };
    let app = runtime_hosts_workflow_app(state);
    register_host(&app, "host-a").await?;

    let result = ActivityResult::failed(
        "remote_check",
        "Remote host reported a failed activity.",
        "remote execution failed",
    );
    let (status, body) = post_json_with_status(
        &app,
        "/api/runtime-hosts/host-a/runtime-jobs/missing-job/complete".to_string(),
        json!({
            "lease_expires_at": chrono::Utc::now(),
            "result": result,
        }),
    )
    .await?;

    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"], "runtime job not found: missing-job");
    Ok(())
}
