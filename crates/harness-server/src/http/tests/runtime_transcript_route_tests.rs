use super::*;
use crate::http::task_mutation_routes::{
    reconstruct_runtime_transcript, RuntimeTranscriptReconstructionRequest,
};
use axum::Json;
use harness_workflow::runtime::{
    ActivityArtifact, ActivityErrorKind, ActivityResult, RuntimeJob, RuntimeKind,
    RuntimeTranscriptRead, WorkflowCommand, WorkflowInstance, WorkflowSubject,
    RUNTIME_TRANSCRIPT_SOURCE_ARTIFACT,
};
use serde_json::json;

#[tokio::test]
async fn transcript_reconstruction_route_accepts_provider_exports_above_axum_default_limit(
) -> anyhow::Result<()> {
    if harness_core::db::resolve_database_url(None).is_err() {
        return Ok(());
    }
    let dir = tempfile::tempdir()?;
    let mut config = harness_core::config::HarnessConfig::default();
    config.server.allow_unauthenticated = true;
    let state = make_test_state_with_workflow_runtime_config_and_registry(
        dir.path(),
        dir.path(),
        config,
        harness_agents::registry::AgentRegistry::new("test"),
    )
    .await?;
    let store = state
        .core
        .workflow_runtime_store
        .as_ref()
        .expect("workflow runtime store")
        .clone();
    let workflow = WorkflowInstance::new(
        "github_issue_pr",
        1,
        "implementing",
        WorkflowSubject::new("issue", "large-export"),
    )
    .with_id("workflow-large-export");
    store.upsert_instance(&workflow).await?;
    let command_id = store
        .enqueue_command(
            &workflow.id,
            None,
            &WorkflowCommand::enqueue_activity("implement_issue", "large-export-command"),
        )
        .await?;
    let job = store
        .enqueue_runtime_job(
            &command_id,
            RuntimeKind::CodexExec,
            "codex-default",
            json!({ "workflow_id": workflow.id, "activity": "implement_issue" }),
        )
        .await?;
    let content = "x".repeat(2 * 1024 * 1024 + 1);
    let body = json!({
        "workflow_id": workflow.id,
        "runtime_job_id": job.id,
        "content": content,
    });
    let app = super::http_router::build_router(state);

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/workflows/runtime/transcripts/reconstruct")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))?,
        )
        .await?;

    let status = response.status();
    let response_body = http_body_util::BodyExt::collect(response.into_body())
        .await?
        .to_bytes();
    assert_eq!(
        status,
        StatusCode::OK,
        "unexpected response: {}",
        String::from_utf8_lossy(&response_body)
    );
    let artifact_ref = harness_workflow::runtime::runtime_transcript_artifact_ref(&job.id);
    let RuntimeTranscriptRead::Verified(record) =
        store.read_runtime_transcript(&artifact_ref).await?
    else {
        anyhow::bail!("large provider export was not stored as a verified transcript");
    };
    assert_eq!(record.content.len(), content.len());
    Ok(())
}

#[tokio::test]
async fn remote_completion_route_accepts_transcripts_above_axum_default_limit() -> anyhow::Result<()>
{
    if harness_core::db::resolve_database_url(None).is_err() {
        return Ok(());
    }
    let dir = tempfile::tempdir()?;
    let mut config = harness_core::config::HarnessConfig::default();
    config.server.allow_unauthenticated = true;
    let state = make_test_state_with_workflow_runtime_config_and_registry(
        dir.path(),
        dir.path(),
        config,
        harness_agents::registry::AgentRegistry::new("test"),
    )
    .await?;
    let store = state
        .core
        .workflow_runtime_store
        .as_ref()
        .expect("workflow runtime store")
        .clone();
    let app = super::http_router::build_router(state);
    crate::handlers::runtime_hosts_workflow_api_tests::register_host(&app, "large-host").await?;
    let job = crate::handlers::runtime_hosts_workflow_api_tests::enqueue_runtime_host_test_job(
        &store,
        "large-transcript",
        RuntimeKind::RemoteHost,
        "remote-host-default",
        json!({
            "activity": "remote_check",
            "workflow_id": "runtime-host-test-large-transcript",
        }),
    )
    .await?;
    let claimed = crate::handlers::runtime_hosts_workflow_api_tests::post_json(
        &app,
        "/api/runtime-hosts/large-host/runtime-jobs/claim".to_string(),
        json!({"lease_secs": 60}),
    )
    .await?;
    let content = "x".repeat(2 * 1024 * 1024 + 1);
    let result = ActivityResult::succeeded("remote_check", "completed").with_artifact(
        ActivityArtifact::new(
            RUNTIME_TRANSCRIPT_SOURCE_ARTIFACT,
            json!({"content": content, "format": "provider.export.v1"}),
        ),
    );
    let body = json!({
        "lease_expires_at": claimed["lease_expires_at"],
        "result": result,
    });

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!(
                    "/api/runtime-hosts/large-host/runtime-jobs/{}/complete",
                    job.id
                ))
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))?,
        )
        .await?;

    assert_eq!(response.status(), StatusCode::OK);
    let artifact_ref = harness_workflow::runtime::runtime_transcript_artifact_ref(&job.id);
    let RuntimeTranscriptRead::Verified(record) =
        store.read_runtime_transcript(&artifact_ref).await?
    else {
        anyhow::bail!("large remote transcript was not stored");
    };
    assert_eq!(record.content.len(), content.len());
    Ok(())
}

fn exact_replay_job(artifact_ref: &str) -> RuntimeJob {
    RuntimeJob::pending(
        "consumer-command",
        RuntimeKind::CodexExec,
        "codex-default",
        json!({
            "workflow_id": "consumer-workflow",
            "activity": "exact_replay",
            "command": {
                "activity": "exact_replay",
                "exact_replay": {
                    "transcript_artifact_ref": artifact_ref,
                },
            },
        }),
    )
}

#[tokio::test]
async fn transcript_reconstruction_route_restores_provider_export() -> anyhow::Result<()> {
    if harness_core::db::resolve_database_url(None).is_err() {
        return Ok(());
    }
    let dir = tempfile::tempdir()?;
    let state = make_test_state_with_workflow_runtime(dir.path()).await?;
    let store = state
        .core
        .workflow_runtime_store
        .as_ref()
        .expect("workflow runtime store");
    let workflow = WorkflowInstance::new(
        "github_issue_pr",
        1,
        "implementing",
        WorkflowSubject::new("issue", "1704"),
    )
    .with_id("transcript-route-workflow");
    store.upsert_instance(&workflow).await?;
    let command_id = store
        .enqueue_command(
            &workflow.id,
            None,
            &WorkflowCommand::enqueue_activity("implement_issue", "transcript-route-command"),
        )
        .await?;
    let job = store
        .enqueue_runtime_job(
            &command_id,
            RuntimeKind::CodexExec,
            "codex-default",
            json!({"workflow_id": workflow.id, "activity": "implement_issue"}),
        )
        .await?;
    let content = "provider transcript export".to_string();
    let expected_checksum = harness_workflow::runtime::runtime_transcript_checksum(&content);

    let (status, Json(body)) = reconstruct_runtime_transcript(
        axum::extract::State(state.clone()),
        Json(RuntimeTranscriptReconstructionRequest {
            workflow_id: workflow.id.clone(),
            runtime_job_id: job.id.clone(),
            content: content.clone(),
            expected_checksum: Some(expected_checksum.clone()),
        }),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "reconstructed");
    assert_eq!(body["checksum"], expected_checksum);
    let artifact_ref = body["artifact_ref"]
        .as_str()
        .expect("artifact_ref response");
    match store.read_runtime_transcript(artifact_ref).await? {
        RuntimeTranscriptRead::Verified(record) => {
            assert_eq!(record.content, content);
            assert_eq!(record.reconstructed_by.as_deref(), Some("operator_api"));
        }
        other => panic!("reconstructed transcript must verify, got {other:?}"),
    }
    Ok(())
}

#[tokio::test]
async fn transcript_reconstruction_route_rejects_wrong_workflow() -> anyhow::Result<()> {
    if harness_core::db::resolve_database_url(None).is_err() {
        return Ok(());
    }
    let dir = tempfile::tempdir()?;
    let state = make_test_state_with_workflow_runtime(dir.path()).await?;
    let store = state
        .core
        .workflow_runtime_store
        .as_ref()
        .expect("workflow runtime store");
    let workflow = WorkflowInstance::new(
        "github_issue_pr",
        1,
        "implementing",
        WorkflowSubject::new("issue", "1704"),
    )
    .with_id("transcript-route-owner");
    store.upsert_instance(&workflow).await?;
    let command_id = store
        .enqueue_command(
            &workflow.id,
            None,
            &WorkflowCommand::enqueue_activity("implement_issue", "transcript-route-owner-command"),
        )
        .await?;
    let job = store
        .enqueue_runtime_job(
            &command_id,
            RuntimeKind::CodexExec,
            "codex-default",
            json!({"workflow_id": workflow.id}),
        )
        .await?;

    let (status, Json(body)) = reconstruct_runtime_transcript(
        axum::extract::State(state),
        Json(RuntimeTranscriptReconstructionRequest {
            workflow_id: "different-workflow".to_string(),
            runtime_job_id: job.id,
            content: "provider transcript export".to_string(),
            expected_checksum: None,
        }),
    )
    .await;

    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(
        body["error"],
        "runtime job does not belong to the requested workflow"
    );
    Ok(())
}

#[tokio::test]
async fn exact_replay_preflight_fails_terminal_on_missing_or_corrupt_transcript(
) -> anyhow::Result<()> {
    if harness_core::db::resolve_database_url(None).is_err() {
        return Ok(());
    }
    let dir = tempfile::tempdir()?;
    let state = make_test_state_with_workflow_runtime(dir.path()).await?;
    let missing_ref = "runtime-transcript:missing";
    let missing = crate::workflow_runtime_worker::exact_replay_preflight_for_test(
        &state,
        &exact_replay_job(missing_ref),
    )
    .await
    .expect("missing transcript must stop before agent dispatch");
    assert_eq!(missing.error_kind, Some(ActivityErrorKind::Fatal));
    assert_eq!(
        missing.signals[0].signal["stop_reason_code"],
        "runtime_transcript_lost"
    );

    let store = state
        .core
        .workflow_runtime_store
        .as_ref()
        .expect("workflow runtime store");
    let workflow = WorkflowInstance::new(
        "github_issue_pr",
        1,
        "implementing",
        WorkflowSubject::new("issue", "1704"),
    )
    .with_id("corrupt-transcript-owner");
    store.upsert_instance(&workflow).await?;
    let command_id = store
        .enqueue_command(
            &workflow.id,
            None,
            &WorkflowCommand::enqueue_activity("implement_issue", "corrupt-transcript-command"),
        )
        .await?;
    let producer = store
        .enqueue_runtime_job(
            &command_id,
            RuntimeKind::CodexExec,
            "codex-default",
            json!({"workflow_id": workflow.id}),
        )
        .await?;
    let reconstructed = store
        .reconstruct_runtime_transcript(
            &workflow.id,
            &producer.id,
            "valid transcript",
            None,
            "test",
        )
        .await?;
    sqlx::query(
        "UPDATE workflow_artifacts
         SET data = jsonb_set(data, '{content}', to_jsonb('corrupt transcript'::text))
         WHERE id = $1",
    )
    .bind(&reconstructed.reference.artifact_ref)
    .execute(store.pool())
    .await?;
    let corrupt = crate::workflow_runtime_worker::exact_replay_preflight_for_test(
        &state,
        &exact_replay_job(&reconstructed.reference.artifact_ref),
    )
    .await
    .expect("corrupt transcript must stop before agent dispatch");
    assert_eq!(corrupt.error_kind, Some(ActivityErrorKind::Fatal));
    assert!(corrupt
        .error
        .as_deref()
        .unwrap_or_default()
        .contains("mismatch"));
    Ok(())
}

#[tokio::test]
async fn exact_replay_hydrates_verified_transcript_before_dispatch() -> anyhow::Result<()> {
    if harness_core::db::resolve_database_url(None).is_err() {
        return Ok(());
    }
    let dir = tempfile::tempdir()?;
    let state = make_test_state_with_workflow_runtime(dir.path()).await?;
    let store = state
        .core
        .workflow_runtime_store
        .as_ref()
        .expect("workflow runtime store");
    let workflow = WorkflowInstance::new(
        "github_issue_pr",
        1,
        "implementing",
        WorkflowSubject::new("issue", "1704"),
    )
    .with_id("hydrate-transcript-owner");
    store.upsert_instance(&workflow).await?;
    let command_id = store
        .enqueue_command(
            &workflow.id,
            None,
            &WorkflowCommand::enqueue_activity("implement_issue", "hydrate-transcript-command"),
        )
        .await?;
    let producer = store
        .enqueue_runtime_job(
            &command_id,
            RuntimeKind::CodexExec,
            "codex-default",
            json!({"workflow_id": workflow.id}),
        )
        .await?;
    let reconstructed = store
        .reconstruct_runtime_transcript(
            &workflow.id,
            &producer.id,
            "verified transcript",
            None,
            "test",
        )
        .await?;
    let mut consumer = exact_replay_job(&reconstructed.reference.artifact_ref);

    crate::workflow_runtime_worker::hydrate_exact_replay_for_test(&state, &mut consumer)
        .await
        .map_err(|result| anyhow::anyhow!(result.error.unwrap_or(result.summary)))?;

    assert_eq!(
        consumer.input["command"]["exact_replay"]["transcript"],
        "verified transcript"
    );
    assert_eq!(
        consumer.input["command"]["exact_replay"]["verified_transcript"]["checksum"],
        reconstructed.reference.checksum
    );
    Ok(())
}
