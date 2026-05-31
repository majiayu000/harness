use super::runtime_hosts;
use axum::{
    body::Body,
    http::{Request, StatusCode},
    routing::post,
    Router,
};
use harness_workflow::runtime::{
    ActivityResult, RuntimeJobStatus, RuntimeKind, WorkflowRuntimeStore,
};
use serde_json::json;
use std::sync::Arc;
use tower::ServiceExt;

fn runtime_hosts_workflow_app(state: Arc<crate::http::AppState>) -> Router {
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
        .with_state(state)
}

async fn make_test_state_with_runtime_store(
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

async fn register_host(app: &Router, host_id: &str) -> anyhow::Result<()> {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/runtime-hosts/register")
                .header("content-type", "application/json")
                .body(Body::from(json!({ "host_id": host_id }).to_string()))?,
        )
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    Ok(())
}

async fn post_json(
    app: &Router,
    uri: String,
    body: serde_json::Value,
) -> anyhow::Result<serde_json::Value> {
    let (status, json) = post_json_with_status(app, uri, body).await?;
    assert_eq!(status, StatusCode::OK);
    Ok(json)
}

async fn post_json_with_status(
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
    Ok((status, serde_json::from_slice(&bytes)?))
}

#[tokio::test]
async fn runtime_job_claim_endpoint_claims_remote_host_jobs_only() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let Some((state, store)) = make_test_state_with_runtime_store(dir.path()).await? else {
        return Ok(());
    };
    let app = runtime_hosts_workflow_app(state);
    register_host(&app, "host-a").await?;

    let local_job = store
        .enqueue_runtime_job(
            "command-local",
            RuntimeKind::CodexJsonrpc,
            "codex-default",
            json!({ "activity": "local_check" }),
        )
        .await?;
    let remote_job = store
        .enqueue_runtime_job(
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
async fn runtime_job_claim_endpoint_blocks_duplicate_claims() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let Some((state, store)) = make_test_state_with_runtime_store(dir.path()).await? else {
        return Ok(());
    };
    let app = runtime_hosts_workflow_app(state);
    register_host(&app, "host-a").await?;
    register_host(&app, "host-b").await?;

    let job = store
        .enqueue_runtime_job(
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
async fn runtime_job_claim_endpoint_reclaims_expired_remote_job() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let Some((state, store)) = make_test_state_with_runtime_store(dir.path()).await? else {
        return Ok(());
    };
    let app = runtime_hosts_workflow_app(state);
    register_host(&app, "host-a").await?;
    register_host(&app, "host-b").await?;

    let job = store
        .enqueue_runtime_job(
            "command-remote",
            RuntimeKind::RemoteHost,
            "remote-host-default",
            json!({ "activity": "remote_check" }),
        )
        .await?;
    let first = post_json(
        &app,
        "/api/runtime-hosts/host-a/runtime-jobs/claim".to_string(),
        json!({ "lease_secs": 0 }),
    )
    .await?;
    assert_eq!(first["runtime_job_id"], job.id);
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;

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

    let job = store
        .enqueue_runtime_job(
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

    let result = ActivityResult::failed(
        "remote_check",
        "Remote host reported a failed activity.",
        "remote execution failed",
    );
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

    let persisted = store
        .get_runtime_job(&job.id)
        .await?
        .expect("runtime job should be persisted");
    assert_eq!(persisted.status, RuntimeJobStatus::Failed);
    assert!(persisted.lease.is_none());
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
