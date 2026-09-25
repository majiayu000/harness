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
use harness_protocol::rest::RUNTIME_JOB_LEASE_PROOF_V1_CAPABILITY;
use harness_workflow::runtime::store::runtime_job_leases::RuntimeJobLeaseRenewalOutcome;
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
    mut capabilities: Vec<&str>,
) -> anyhow::Result<()> {
    if !capabilities.contains(&RUNTIME_JOB_LEASE_PROOF_V1_CAPABILITY) {
        capabilities.push(RUNTIME_JOB_LEASE_PROOF_V1_CAPABILITY);
    }
    register_host_with_exact_capabilities(app, host_id, capabilities).await
}

async fn register_host_with_exact_capabilities(
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

include!("runtime_hosts_workflow_claim_cases.rs");
include!("runtime_hosts_workflow_completion_fencing_cases.rs");
include!("runtime_hosts_workflow_lease_host_lifecycle_cases.rs");

#[path = "runtime_hosts_terminal_fence_cases.rs"]
mod terminal_fence_cases;
