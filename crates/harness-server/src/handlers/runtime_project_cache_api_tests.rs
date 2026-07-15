use super::{runtime_hosts, runtime_project_cache};
use crate::project_registry::Project;
use crate::services::project::ProjectService;
use async_trait::async_trait;
use axum::{
    body::Body,
    http::{Request, StatusCode},
    routing::{get, post},
    Router,
};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Barrier;
use tower::ServiceExt;

struct BlockingResolveProjectService {
    root: PathBuf,
    entered: Arc<Barrier>,
    release: Arc<Barrier>,
}

#[async_trait]
impl ProjectService for BlockingResolveProjectService {
    async fn register(&self, _project: Project) -> anyhow::Result<()> {
        Ok(())
    }

    async fn get(&self, _id: &str) -> anyhow::Result<Option<Project>> {
        Ok(None)
    }

    async fn get_by_name(&self, _name: &str) -> anyhow::Result<Option<Project>> {
        Ok(None)
    }

    async fn list(&self) -> anyhow::Result<Vec<Project>> {
        Ok(Vec::new())
    }

    async fn remove(&self, _id: &str) -> anyhow::Result<bool> {
        Ok(false)
    }

    async fn resolve_path(&self, id: &str) -> anyhow::Result<Option<PathBuf>> {
        if id != "slow-project" {
            return Ok(None);
        }
        self.entered.wait().await;
        self.release.wait().await;
        Ok(Some(self.root.clone()))
    }

    fn default_root(&self) -> &Path {
        &self.root
    }
}

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

async fn make_test_state(
    dir: &std::path::Path,
) -> anyhow::Result<Option<Arc<crate::http::AppState>>> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(None);
    }
    match crate::test_helpers::make_test_state(dir).await {
        Ok(state) => Ok(Some(Arc::new(state))),
        Err(err) if crate::test_helpers::is_pool_timeout(&err) => Ok(None),
        Err(err) => Err(err),
    }
}

#[tokio::test]
async fn sync_and_list_watched_projects_by_path() -> anyhow::Result<()> {
    let data_dir = tempfile::tempdir()?;
    let project_dir = tempfile::tempdir()?;
    std::fs::create_dir_all(project_dir.path().join(".git"))?;

    let Some(state) = make_test_state(data_dir.path()).await? else {
        return Ok(());
    };
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
    let Some(state) = make_test_state(dir.path()).await? else {
        return Ok(());
    };
    let app = runtime_project_cache_app(state);

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
async fn sync_rejects_required_missing_runtime_state_store_before_cache_mutation(
) -> anyhow::Result<()> {
    let data_dir = tempfile::tempdir()?;
    let project_dir = tempfile::tempdir()?;
    std::fs::create_dir_all(project_dir.path().join(".git"))?;

    let Some(mut state) = make_test_state(data_dir.path()).await? else {
        return Ok(());
    };
    state
        .runtime_hosts
        .register("host-a".to_string(), None, vec![]);
    let state_mut =
        Arc::get_mut(&mut state).ok_or_else(|| anyhow::anyhow!("expected unique state"))?;
    state_mut.startup_statuses =
        vec![
            crate::http::state::StoreStartupResult::optional("runtime_state_store")
                .failed("pool timed out while waiting for an open connection"),
        ];
    state_mut.degraded_subsystems = vec!["runtime_state_store"];

    let app = runtime_project_cache_app(state.clone());
    let response = app
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

    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body = http_body_util::BodyExt::collect(response.into_body())
        .await?
        .to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body)?;
    assert_eq!(json["error"], "runtime state persistence unavailable");
    assert!(state
        .runtime_project_cache
        .get_host_cache("host-a")
        .is_none());
    assert!(state.is_runtime_state_dirty());
    Ok(())
}

#[tokio::test]
async fn sync_by_project_id_resolves_registry_root() -> anyhow::Result<()> {
    let data_dir = tempfile::tempdir()?;
    let project_dir = tempfile::tempdir()?;
    std::fs::create_dir_all(project_dir.path().join(".git"))?;
    let Some(state) = make_test_state(data_dir.path()).await? else {
        return Ok(());
    };
    state
        .project_svc
        .register(crate::project_registry::Project {
            id: "demo".to_string(),
            root: project_dir.path().to_path_buf(),
            name: None,
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

    let Some((state, _runtime_store)) =
        super::runtime_hosts_workflow_api_tests::make_test_state_with_runtime_store(
            data_dir.path(),
        )
        .await?
    else {
        return Ok(());
    };
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

#[tokio::test]
async fn draining_host_cannot_repopulate_cache_after_partial_deregister() -> anyhow::Result<()> {
    let data_dir = tempfile::tempdir()?;
    let project_dir = tempfile::tempdir()?;
    std::fs::create_dir_all(project_dir.path().join(".git"))?;

    let Some((state, runtime_store)) =
        super::runtime_hosts_workflow_api_tests::make_test_state_with_runtime_store(
            data_dir.path(),
        )
        .await?
    else {
        return Ok(());
    };
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

    sqlx::query("DROP TABLE runtime_jobs CASCADE")
        .execute(runtime_store.pool())
        .await?;
    let deregister = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/runtime-hosts/host-a/deregister")
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(deregister.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        state.runtime_hosts.lifecycle("host-a"),
        Some(crate::runtime_hosts::RuntimeHostLifecycle::Draining)
    );

    state.runtime_project_cache.clear_host("host-a");
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
    assert_eq!(stale_sync.status(), StatusCode::CONFLICT);
    assert!(state
        .runtime_project_cache
        .get_host_cache("host-a")
        .is_none());
    Ok(())
}

#[tokio::test]
async fn project_resolution_releases_host_lock_and_rechecks_draining() -> anyhow::Result<()> {
    let data_dir = tempfile::tempdir()?;
    let project_dir = tempfile::tempdir()?;
    std::fs::create_dir_all(project_dir.path().join(".git"))?;
    let entered = Arc::new(Barrier::new(2));
    let release = Arc::new(Barrier::new(2));

    let Some((mut state, _runtime_store)) =
        super::runtime_hosts_workflow_api_tests::make_test_state_with_runtime_store(
            data_dir.path(),
        )
        .await?
    else {
        return Ok(());
    };
    Arc::get_mut(&mut state)
        .ok_or_else(|| anyhow::anyhow!("expected unique test state"))?
        .project_svc = Arc::new(BlockingResolveProjectService {
        root: project_dir.path().to_path_buf(),
        entered: entered.clone(),
        release: release.clone(),
    });
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

    let sync_app = app.clone();
    let sync_request = Request::builder()
        .method("POST")
        .uri("/api/runtime-hosts/host-a/projects/sync")
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::json!({"projects": [{"project": "slow-project"}]}).to_string(),
        ))?;
    let sync = tokio::spawn(async move { sync_app.oneshot(sync_request).await });
    entered.wait().await;

    let host_lock_available = match tokio::time::timeout(
        Duration::from_millis(250),
        state.runtime_hosts.lock_operation("host-a"),
    )
    .await
    {
        Ok(_host_operation) => {
            assert_eq!(
                state.runtime_hosts.mark_draining("host-a"),
                Some(crate::runtime_hosts::RuntimeHostLifecycle::Active)
            );
            true
        }
        Err(_) => false,
    };
    release.wait().await;
    let response = sync.await??;

    assert!(
        host_lock_available,
        "project resolution must not hold the host operation lock"
    );
    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert!(state
        .runtime_project_cache
        .get_host_cache("host-a")
        .is_none());
    Ok(())
}
