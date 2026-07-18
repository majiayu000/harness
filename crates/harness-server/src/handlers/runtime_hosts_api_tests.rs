use super::runtime_hosts;
use axum::{
    body::Body,
    http::Request,
    routing::{get, post},
    Router,
};
use harness_workflow::runtime::WorkflowRuntimeStore;
use std::sync::Arc;
use tower::ServiceExt;

fn runtime_hosts_app(state: Arc<crate::http::AppState>) -> Router {
    Router::new()
        .route("/api/runtime-hosts", get(runtime_hosts::list_runtime_hosts))
        .route(
            "/api/runtime-hosts/register",
            post(runtime_hosts::register_runtime_host),
        )
        .route(
            "/api/runtime-hosts/{id}/heartbeat",
            post(runtime_hosts::heartbeat_runtime_host),
        )
        .route(
            "/api/runtime-hosts/{id}/deregister",
            post(runtime_hosts::deregister_runtime_host),
        )
        .with_state(state)
}

async fn make_test_state(
    dir: &std::path::Path,
) -> anyhow::Result<Option<Arc<crate::http::AppState>>> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(None);
    }
    match crate::test_helpers::make_test_state(dir).await {
        Ok(state) => {
            let store =
                Arc::new(WorkflowRuntimeStore::open(&dir.join("workflow_runtime.db")).await?);
            let mut state = Arc::new(state);
            Arc::get_mut(&mut state)
                .ok_or_else(|| anyhow::anyhow!("expected unique test state"))?
                .core
                .workflow_runtime_store = Some(store);
            Ok(Some(state))
        }
        Err(err) if crate::test_helpers::is_pool_timeout(&err) => Ok(None),
        Err(err) => Err(err),
    }
}

#[tokio::test]
async fn register_then_list_runtime_hosts() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let Some(state) = make_test_state(dir.path()).await? else {
        return Ok(());
    };
    let app = runtime_hosts_app(state);

    let body = serde_json::json!({
        "host_id": "host-a",
        "display_name": "Host A",
        "capabilities": ["claude", "codex"]
    });
    let register = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/runtime-hosts/register")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))?,
        )
        .await?;
    assert_eq!(register.status(), axum::http::StatusCode::OK);

    let list = app
        .oneshot(
            Request::builder()
                .uri("/api/runtime-hosts")
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(list.status(), axum::http::StatusCode::OK);
    let data = http_body_util::BodyExt::collect(list.into_body())
        .await?
        .to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&data)?;
    assert_eq!(json["hosts"][0]["id"], "host-a");
    assert_eq!(json["hosts"][0]["online"], true);
    Ok(())
}
