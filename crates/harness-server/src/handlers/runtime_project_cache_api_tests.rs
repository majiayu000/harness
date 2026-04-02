use super::{runtime_hosts, runtime_project_cache};
use axum::{
    body::Body,
    http::{Request, StatusCode},
    routing::{get, post},
    Router,
};
use std::sync::Arc;
use tower::ServiceExt;

fn runtime_project_cache_app(state: Arc<crate::http::AppState>) -> Router {
    Router::new()
        .route(
            "/api/runtime-hosts/register",
            post(runtime_hosts::register_runtime_host),
        )
        .route(
            "/api/runtime-hosts/{host_id}/deregister",
            post(runtime_hosts::deregister_runtime_host),
        )
        .route(
            "/api/runtime-hosts/{host_id}/projects",
            get(runtime_project_cache::list_runtime_host_projects),
        )
        .route(
            "/api/runtime-hosts/{host_id}/projects/sync",
            post(runtime_project_cache::sync_runtime_host_projects),
        )
        .with_state(state)
}

#[tokio::test]
async fn sync_and_list_watched_projects_by_path() -> anyhow::Result<()> {
    let data_dir = tempfile::tempdir()?;
    let project_dir = tempfile::tempdir()?;
    std::fs::create_dir_all(project_dir.path().join(".git"))?;

    let state = Arc::new(crate::test_helpers::make_test_state(data_dir.path()).await?);
    let app = runtime_project_cache_app(state);

    let register = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/runtime-hosts/register")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({"host_id": "host-a"}).to_string(),
                ))?,
        )
        .await?;
    assert_eq!(register.status(), StatusCode::OK);

    let sync = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/runtime-hosts/host-a/projects/sync")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "projects": [{"project": project_dir.path().to_string_lossy()}]
                    })
                    .to_string(),
                ))?,
        )
        .await?;
    assert_eq!(sync.status(), StatusCode::OK);

    let list = app
        .oneshot(
            Request::builder()
                .uri("/api/runtime-hosts/host-a/projects")
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(list.status(), StatusCode::OK);
    let body = http_body_util::BodyExt::collect(list.into_body())
        .await?
        .to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body)?;
    assert_eq!(json["project_count"], 1);
    assert_eq!(
        json["projects"][0]["root"],
        project_dir
            .path()
            .canonicalize()?
            .to_string_lossy()
            .to_string()
    );
    Ok(())
}

#[tokio::test]
async fn sync_unknown_host_returns_not_found() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let app = runtime_project_cache_app(Arc::new(
        crate::test_helpers::make_test_state(dir.path()).await?,
    ));

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/runtime-hosts/ghost/projects/sync")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({ "projects": [{"project": "."}] }).to_string(),
                ))?,
        )
        .await?;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    Ok(())
}

#[tokio::test]
async fn sync_by_project_id_resolves_registry_root() -> anyhow::Result<()> {
    let data_dir = tempfile::tempdir()?;
    let project_dir = tempfile::tempdir()?;
    std::fs::create_dir_all(project_dir.path().join(".git"))?;
    let state = Arc::new(crate::test_helpers::make_test_state(data_dir.path()).await?);
    state
        .project_svc
        .register(crate::project_registry::Project {
            id: "demo".to_string(),
            root: project_dir.path().to_path_buf(),
            max_concurrent: None,
            default_agent: None,
            active: true,
            created_at: chrono::Utc::now().to_rfc3339(),
        })
        .await?;

    let app = runtime_project_cache_app(state);
    let register = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/runtime-hosts/register")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({"host_id": "host-a"}).to_string(),
                ))?,
        )
        .await?;
    assert_eq!(register.status(), StatusCode::OK);

    let sync = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/runtime-hosts/host-a/projects/sync")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({ "projects": [{"project": "demo"}] }).to_string(),
                ))?,
        )
        .await?;
    assert_eq!(sync.status(), StatusCode::OK);
    let json: serde_json::Value = serde_json::from_slice(
        &http_body_util::BodyExt::collect(sync.into_body())
            .await?
            .to_bytes(),
    )?;
    assert_eq!(json["projects"][0]["project_id"], "demo");
    Ok(())
}

#[tokio::test]
async fn sync_after_deregister_does_not_recreate_cache() -> anyhow::Result<()> {
    let data_dir = tempfile::tempdir()?;
    let project_dir = tempfile::tempdir()?;
    std::fs::create_dir_all(project_dir.path().join(".git"))?;

    let state = Arc::new(crate::test_helpers::make_test_state(data_dir.path()).await?);
    let app = runtime_project_cache_app(state.clone());

    let register = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/runtime-hosts/register")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({"host_id": "host-a"}).to_string(),
                ))?,
        )
        .await?;
    assert_eq!(register.status(), StatusCode::OK);

    let initial_sync = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/runtime-hosts/host-a/projects/sync")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "projects": [{"project": project_dir.path().to_string_lossy()}]
                    })
                    .to_string(),
                ))?,
        )
        .await?;
    assert_eq!(initial_sync.status(), StatusCode::OK);

    let deregister = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/runtime-hosts/host-a/deregister")
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(deregister.status(), StatusCode::OK);
    assert!(state
        .runtime_project_cache
        .get_host_cache("host-a")
        .is_none());

    let stale_sync = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/runtime-hosts/host-a/projects/sync")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "projects": [{"project": project_dir.path().to_string_lossy()}]
                    })
                    .to_string(),
                ))?,
        )
        .await?;
    assert_eq!(stale_sync.status(), StatusCode::NOT_FOUND);
    assert!(state
        .runtime_project_cache
        .get_host_cache("host-a")
        .is_none());
    Ok(())
}
