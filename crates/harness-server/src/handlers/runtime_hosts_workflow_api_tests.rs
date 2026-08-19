use super::runtime_hosts;
use axum::{
    body::Body,
    http::{Request, StatusCode},
    routing::post,
    Router,
};
use serde_json::json;
use std::sync::Arc;
use tower::ServiceExt;

use chrono::Utc;
use harness_workflow::runtime::{
    ActivityArtifact, ActivityResult, ActivitySignal, RuntimeJob, RuntimeJobStatus, RuntimeKind,
    RuntimeTranscriptRead, WorkflowCommand, WorkflowInstance, WorkflowRuntimeStore,
    WorkflowSubject, RUNTIME_TRANSCRIPT_ARTIFACT, RUNTIME_TRANSCRIPT_SOURCE_ARTIFACT,
};

pub(super) fn runtime_hosts_workflow_app(state: Arc<crate::http::AppState>) -> Router {
    Router::new()
        .route(
            "/api/runtime-hosts/register",
            post(runtime_hosts::register_runtime_host),
        )
        .route(
            "/api/runtime-hosts/{id}/runtime-jobs/claim",
            post(runtime_hosts::claim_runtime_job_for_runtime_host),
        )
        .route(
            "/api/runtime-hosts/{id}/runtime-jobs/{runtime_job_id}/complete",
            post(runtime_hosts::complete_runtime_job_for_runtime_host),
        )
        .route(
            "/api/runtime-hosts/{id}/runtime-jobs/{runtime_job_id}/lease/renew",
            post(runtime_hosts::renew_runtime_job_lease_for_runtime_host),
        )
        .route(
            "/api/runtime-hosts/{id}/deregister",
            post(runtime_hosts::deregister_runtime_host),
        )
        .with_state(state)
}

pub(super) async fn make_test_state_with_runtime_store(
    dir: &std::path::Path,
) -> anyhow::Result<Option<(Arc<crate::http::AppState>, Arc<WorkflowRuntimeStore>)>> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(None);
    }
    let state = match crate::test_helpers::make_test_state(dir).await {
        Ok(state) => state,
        Err(err) if crate::test_helpers::is_pool_timeout(&err) => return Ok(None),
        Err(err) => return Err(err),
    };
    let store = Arc::new(WorkflowRuntimeStore::open(&dir.join("workflow_runtime.db")).await?);
    let mut state = Arc::new(state);
    Arc::get_mut(&mut state)
        .ok_or_else(|| anyhow::anyhow!("expected unique test state"))?
        .core
        .workflow_runtime_store = Some(store.clone());
    Ok(Some((state, store)))
}

pub(crate) async fn register_host(app: &Router, host_id: &str) -> anyhow::Result<()> {
    register_host_with_capabilities(app, host_id, Vec::new()).await
}

pub(crate) async fn register_host_with_capabilities(
    app: &Router,
    host_id: &str,
    capabilities: Vec<&str>,
) -> anyhow::Result<()> {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/runtime-hosts/register")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({ "host_id": host_id, "capabilities": capabilities }).to_string(),
                ))?,
        )
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    Ok(())
}

pub(crate) async fn enqueue_runtime_host_test_job(
    store: &WorkflowRuntimeStore,
    key: &str,
    runtime_kind: RuntimeKind,
    runtime_profile: &str,
    input: serde_json::Value,
) -> anyhow::Result<RuntimeJob> {
    let workflow = WorkflowInstance::new(
        "github_issue_pr",
        1,
        "implementing",
        WorkflowSubject::new("issue", format!("issue:{key}")),
    )
    .with_id(format!("runtime-host-test-{key}"));
    crate::test_helpers::force_upsert_runtime_lifecycle_state_for_test(store, &workflow).await?;
    let activity = input
        .get("activity")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("remote_check");
    let command = WorkflowCommand::enqueue_activity(activity, format!("runtime-host-test-{key}"));
    let command_id = store.enqueue_command(&workflow.id, None, &command).await?;
    store
        .enqueue_runtime_job(&command_id, runtime_kind, runtime_profile, input)
        .await
}

pub(crate) async fn post_json(
    app: &Router,
    uri: String,
    body: serde_json::Value,
) -> anyhow::Result<serde_json::Value> {
    let (status, json) = post_json_with_status(app, uri, body).await?;
    assert_eq!(status, StatusCode::OK);
    Ok(json)
}

pub(super) async fn post_json_with_status(
    app: &Router,
    uri: String,
    body: serde_json::Value,
) -> anyhow::Result<(StatusCode, serde_json::Value)> {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(uri)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))?,
        )
        .await?;
    let status = response.status();
    let bytes = http_body_util::BodyExt::collect(response.into_body())
        .await?
        .to_bytes();
    let json = serde_json::from_slice(&bytes).map_err(|error| {
        anyhow::anyhow!(
            "failed to decode HTTP {status} response as JSON (body: {:?}): {error}",
            String::from_utf8_lossy(&bytes)
        )
    })?;
    Ok((status, json))
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
            "lease_expires_at": lease_expires_at,
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
async fn runtime_job_completion_preserves_cancelled_eval_feedback_cleanup_proof(
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let Some((state, store)) = make_test_state_with_runtime_store(dir.path()).await? else {
        return Ok(());
    };
    let app = runtime_hosts_workflow_app(state);
    register_host_with_capabilities(&app, "host-a", vec!["eval_resource_limits"]).await?;
    let job = enqueue_runtime_host_test_job(
        &store,
        "cancelled-eval-feedback",
        RuntimeKind::RemoteHost,
        "remote-host-default",
        json!({
            "activity": harness_workflow::runtime::PR_FEEDBACK_INSPECT_ACTIVITY,
            "command": {
                "activity": harness_workflow::runtime::PR_FEEDBACK_INSPECT_ACTIVITY,
                "eval": {
                    "eval_run_id": "run-cancelled-feedback",
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
    store
        .cancel_command_and_unfinished_runtime_jobs(
            &job.command_id,
            harness_workflow::runtime::PR_FEEDBACK_INSPECT_ACTIVITY,
            "operator cancelled eval",
        )
        .await?;

    let completed = post_json(
        &app,
        format!("/api/runtime-hosts/host-a/runtime-jobs/{}/complete", job.id),
        json!({
            "lease_generation": claimed["lease_generation"],
            "lease_expires_at": claimed["lease_expires_at"],
            "result": ActivityResult::cancelled(
                harness_workflow::runtime::PR_FEEDBACK_INSPECT_ACTIVITY,
                "host stopped and cleaned",
            ),
            "execution_evidence": {
                "checked_out_commit": "",
                "resource_limit_report": {},
                "usage": {
                    "model": "test-model",
                    "input_tokens": 0,
                    "output_tokens": 0,
                    "cached_input_tokens": 0,
                    "total_tokens": 0,
                    "cost_usd_micros": 0
                },
                "isolation_cleanup_status": "cleaned",
                "validation": []
            }
        }),
    )
    .await?;

    assert_eq!(completed["completed"], true);
    assert_eq!(completed["runtime_job"]["status"], "cancelled");
    assert!(completed["runtime_job"]["output"]["artifacts"]
        .as_array()
        .is_some_and(|artifacts| artifacts.iter().any(|artifact| {
            artifact["artifact_type"]
                == harness_workflow::runtime::completion_evidence::ARTIFACT_EVAL_ISOLATION_CLEANUP
                && artifact["artifact"]["status"] == "cleaned"
                && artifact["artifact"]["evidence_source"] == "runtime_host_cancellation_ack"
        })));
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
            "lease_expires_at": claimed["lease_expires_at"],
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
            "lease_expires_at": lease_expires_at,
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

#[tokio::test]
async fn runtime_job_lease_renewal_is_fenced_idempotent_and_sanitized() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let Some((state, store)) = make_test_state_with_runtime_store(dir.path()).await? else {
        return Ok(());
    };
    let app = runtime_hosts_workflow_app(state);
    register_host(&app, "host-a").await?;
    register_host(&app, "host-b").await?;
    let job = enqueue_runtime_host_test_job(
        &store,
        "renewal",
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
    let request = json!({
        "lease_generation": claimed["lease_generation"],
        "lease_expires_at": claimed["lease_expires_at"],
        "renewal_id": uuid::Uuid::new_v4(),
        "lease_secs": 120,
    });
    let uri = format!(
        "/api/runtime-hosts/host-a/runtime-jobs/{}/lease/renew",
        job.id
    );
    let renewed = post_json(&app, uri.clone(), request.clone()).await?;
    assert_eq!(renewed["renewed"], true);
    assert_eq!(renewed["replayed"], false);
    assert_eq!(renewed["lease_generation"], claimed["lease_generation"]);

    let replayed = post_json(&app, uri, request.clone()).await?;
    assert_eq!(replayed["replayed"], true);
    assert_eq!(replayed["lease_expires_at"], renewed["lease_expires_at"]);

    let lease_generation = claimed["lease_generation"]
        .as_u64()
        .ok_or_else(|| anyhow::anyhow!("claim must return a numeric lease generation"))?;
    let result = ActivityResult::failed(
        "remote_check",
        "Remote host reported a failed activity.",
        "remote execution failed",
    );
    let (status, _) = post_json_with_status(
        &app,
        format!("/api/runtime-hosts/host-a/runtime-jobs/{}/complete", job.id),
        json!({
            "lease_generation": lease_generation + 1,
            "lease_expires_at": renewed["lease_expires_at"],
            "result": result,
        }),
    )
    .await?;
    assert_eq!(status, StatusCode::CONFLICT);

    let (status, lost) = post_json_with_status(
        &app,
        format!(
            "/api/runtime-hosts/host-b/runtime-jobs/{}/lease/renew",
            job.id
        ),
        request,
    )
    .await?;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(
        lost,
        json!({ "error_code": "lease_lost", "must_stop": true })
    );
    assert!(!lost.to_string().contains("host-a"));

    let completed = post_json(
        &app,
        format!("/api/runtime-hosts/host-a/runtime-jobs/{}/complete", job.id),
        json!({
            "lease_generation": lease_generation,
            "lease_expires_at": renewed["lease_expires_at"],
            "result": ActivityResult::failed(
                "remote_check",
                "Remote host reported a failed activity.",
                "remote execution failed",
            ),
        }),
    )
    .await?;
    assert_eq!(completed["completed"], true);
    Ok(())
}

#[tokio::test]
async fn runtime_job_lease_renewal_rejects_invalid_duration_as_bad_request() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let Some((state, _store)) = make_test_state_with_runtime_store(dir.path()).await? else {
        return Ok(());
    };
    let app = runtime_hosts_workflow_app(state);
    register_host(&app, "host-a").await?;

    for lease_secs in [json!(0), json!(3601), json!(null)] {
        let (status, _) = post_json_with_status(
            &app,
            "/api/runtime-hosts/host-a/runtime-jobs/missing/lease/renew".to_string(),
            json!({
                "lease_generation": 1,
                "lease_expires_at": Utc::now(),
                "renewal_id": uuid::Uuid::new_v4(),
                "lease_secs": lease_secs,
            }),
        )
        .await?;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }
    Ok(())
}

#[tokio::test]
async fn runtime_host_deregister_revokes_workflow_job_before_removal() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let Some((state, store)) = make_test_state_with_runtime_store(dir.path()).await? else {
        return Ok(());
    };
    let app = runtime_hosts_workflow_app(state.clone());
    register_host(&app, "host-a").await?;
    let job = enqueue_runtime_host_test_job(
        &store,
        "deregister-revocation",
        RuntimeKind::RemoteHost,
        "remote-host-default",
        json!({ "activity": "remote_check" }),
    )
    .await?;
    post_json(
        &app,
        "/api/runtime-hosts/host-a/runtime-jobs/claim".to_string(),
        json!({ "lease_secs": 60 }),
    )
    .await?;

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
    assert!(!state.runtime_hosts.hosts.contains_key("host-a"));
    let persisted = store
        .get_runtime_job(&job.id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("runtime job should remain persisted"))?;
    assert_eq!(persisted.status, RuntimeJobStatus::Pending);
    assert!(persisted.lease.is_none());
    let events = store.runtime_events_for(&job.id).await?;
    assert_eq!(
        events.last().map(|event| event.event_type.as_str()),
        Some("RuntimeJobLeaseRevoked")
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
    let app = runtime_hosts_workflow_app(state.clone());

    let (status, body) = post_json_with_status(
        &app,
        "/api/runtime-hosts/register".to_string(),
        json!({
            "host_id": "host-a",
            "display_name": null,
            "capabilities": [],
        }),
    )
    .await?;

    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["error"], "runtime state persistence unavailable");
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
    let app = runtime_hosts_workflow_app(state.clone());

    let (status, body) = post_json_with_status(
        &app,
        "/api/runtime-hosts/ghost-host/deregister".to_string(),
        json!({}),
    )
    .await?;

    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["error"], "runtime state persistence unavailable");
    assert!(state.is_runtime_state_dirty());
    Ok(())
}
