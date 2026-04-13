use super::*;
use crate::{server::HarnessServer, thread_manager::ThreadManager};
use harness_agents::registry::AgentRegistry;
use harness_core::config::HarnessConfig;
use std::sync::Arc;

fn make_server(
    project_root: std::path::PathBuf,
    data_dir: std::path::PathBuf,
) -> Arc<HarnessServer> {
    let mut config = HarnessConfig::default();
    config.server.project_root = project_root;
    config.server.data_dir = data_dir;
    Arc::new(HarnessServer::new(
        config,
        ThreadManager::new(),
        AgentRegistry::new("test"),
    ))
}

#[tokio::test]
async fn bootstrap_clamps_zero_notification_capacity() -> anyhow::Result<()> {
    let sandbox = tempfile::tempdir()?;
    let project_root = sandbox.path().join("project");
    std::fs::create_dir_all(&project_root)?;
    let data_dir = sandbox.path().join("data");

    let mut config = HarnessConfig::default();
    config.server.project_root = project_root.clone();
    config.server.data_dir = data_dir.clone();
    config.server.notification_broadcast_capacity = 0;
    let server = Arc::new(HarnessServer::new(
        config,
        ThreadManager::new(),
        AgentRegistry::new("test"),
    ));

    let bootstrap = bootstrap(&server)?;

    assert_eq!(bootstrap.notification_broadcast_capacity, 1);
    Ok(())
}

#[tokio::test]
async fn bootstrap_rejects_invalid_project_root() {
    let sandbox = tempfile::tempdir().expect("tempdir");
    let missing = sandbox.path().join("missing");
    let data_dir = sandbox.path().join("data");
    let server = make_server(missing, data_dir);

    let err = bootstrap(&server).expect_err("bootstrap should fail");
    assert!(
        err.to_string().contains("invalid server.project_root"),
        "unexpected error: {err}"
    );
}

#[tokio::test]
async fn init_persistence_tolerates_q_value_store_failure() -> anyhow::Result<()> {
    let sandbox = tempfile::tempdir()?;
    let project_root = sandbox.path().join("project");
    std::fs::create_dir_all(&project_root)?;
    let data_dir = sandbox.path().join("data");
    std::fs::create_dir_all(&data_dir)?;
    std::fs::create_dir_all(data_dir.join("q_values.db"))?;
    let server = make_server(project_root, data_dir);
    let bootstrap = bootstrap(&server)?;

    let stores = init_persistence(&bootstrap).await?;

    assert!(stores.q_values.is_none());
    Ok(())
}

#[tokio::test]
async fn init_rules_and_skills_loads_builtin_guard() -> anyhow::Result<()> {
    let sandbox = tempfile::tempdir()?;
    let project_root = sandbox.path().join("project");
    std::fs::create_dir_all(&project_root)?;
    let data_dir = sandbox.path().join("data");
    let server = make_server(project_root, data_dir);
    let bootstrap = bootstrap(&server)?;

    let engines = init_rules_and_skills(&server, &bootstrap)?;
    let rules = engines.rules.read().await;

    assert!(rules
        .guards()
        .iter()
        .any(|guard| { guard.id.as_str() == harness_rules::engine::BUILTIN_BASELINE_GUARD_ID }));
    Ok(())
}

#[tokio::test]
async fn init_observability_tolerates_review_store_failure() -> anyhow::Result<()> {
    let sandbox = tempfile::tempdir()?;
    let project_root = sandbox.path().join("project");
    std::fs::create_dir_all(&project_root)?;
    let data_dir = sandbox.path().join("data");
    std::fs::create_dir_all(&data_dir)?;
    std::fs::create_dir_all(data_dir.join("reviews.db"))?;
    let server = make_server(project_root, data_dir);
    let bootstrap = bootstrap(&server)?;

    let observability = init_observability(&server, &bootstrap).await?;

    assert!(observability.review_store.is_none());
    Ok(())
}

#[tokio::test]
async fn hydrate_caches_registers_startup_projects() -> anyhow::Result<()> {
    let sandbox = tempfile::tempdir()?;
    let project_root = sandbox.path().join("project");
    let extra_root = sandbox.path().join("extra");
    std::fs::create_dir_all(&project_root)?;
    std::fs::create_dir_all(&extra_root)?;
    let data_dir = sandbox.path().join("data");

    let mut config = HarnessConfig::default();
    config.server.project_root = project_root.clone();
    config.server.data_dir = data_dir.clone();
    let mut server = HarnessServer::new(config, ThreadManager::new(), AgentRegistry::new("test"));
    server
        .startup_projects
        .push(("extra".to_string(), extra_root.clone()));
    let server = Arc::new(server);

    let bootstrap = bootstrap(&server)?;
    let stores = init_persistence(&bootstrap).await?;
    let cache = hydrate_caches_and_projects(&server, &bootstrap, &stores).await?;

    let default = stores
        .project_registry
        .get("default")
        .await?
        .expect("default project should be registered");
    let extra = stores
        .project_registry
        .get("extra")
        .await?
        .expect("startup project should be registered");

    assert_eq!(default.root, project_root.canonicalize()?);
    assert_eq!(extra.root, extra_root);
    assert!(cache.plan_cache.is_empty());
    Ok(())
}

#[tokio::test]
async fn init_concurrency_tolerates_workspace_manager_failure() -> anyhow::Result<()> {
    let sandbox = tempfile::tempdir()?;
    let project_root = sandbox.path().join("project");
    std::fs::create_dir_all(&project_root)?;
    let data_dir = sandbox.path().join("data");
    let workspace_root = sandbox.path().join("workspace-root");
    std::fs::create_dir_all(&workspace_root)?;

    let mut config = HarnessConfig::default();
    config.server.project_root = project_root.clone();
    config.server.data_dir = data_dir.clone();
    config.workspace.root = workspace_root.clone();
    let server = Arc::new(HarnessServer::new(
        config,
        ThreadManager::new(),
        AgentRegistry::new("test"),
    ));
    let bootstrap = bootstrap(&server)?;
    let stores = init_persistence(&bootstrap).await?;

    std::fs::remove_dir_all(&workspace_root)?;
    std::fs::write(&workspace_root, b"not a directory")?;

    let concurrency = init_concurrency(&server, &bootstrap, &stores.tasks).await?;

    assert!(concurrency.workspace_mgr.is_none());
    Ok(())
}

#[tokio::test]
async fn restore_runtime_state_handles_missing_snapshot() -> anyhow::Result<()> {
    let sandbox = tempfile::tempdir()?;
    let project_root = sandbox.path().join("project");
    std::fs::create_dir_all(&project_root)?;
    let data_dir = sandbox.path().join("data");
    let server = make_server(project_root, data_dir);
    let bootstrap = bootstrap(&server)?;

    let runtime = restore_runtime_state(&bootstrap).await?;

    assert!(runtime.runtime_state_store.is_some());
    assert!(runtime.runtime_hosts.snapshot_state().0.is_empty());
    assert!(runtime.runtime_project_cache.snapshot_state().is_empty());
    Ok(())
}

#[tokio::test]
async fn restore_runtime_state_tolerates_store_open_failure() -> anyhow::Result<()> {
    let sandbox = tempfile::tempdir()?;
    let project_root = sandbox.path().join("project");
    std::fs::create_dir_all(&project_root)?;
    let data_dir = sandbox.path().join("data");
    std::fs::create_dir_all(&data_dir)?;
    std::fs::create_dir_all(data_dir.join("runtime_state.db"))?;
    let server = make_server(project_root, data_dir);
    let bootstrap = bootstrap(&server)?;

    let runtime = restore_runtime_state(&bootstrap).await?;

    assert!(runtime.runtime_state_store.is_none());
    assert!(runtime.runtime_hosts.snapshot_state().0.is_empty());
    assert!(runtime.runtime_project_cache.snapshot_state().is_empty());
    Ok(())
}

#[tokio::test]
async fn restore_runtime_state_ignores_snapshot_load_failure() -> anyhow::Result<()> {
    let sandbox = tempfile::tempdir()?;
    let project_root = sandbox.path().join("project");
    std::fs::create_dir_all(&project_root)?;
    let data_dir = sandbox.path().join("data");
    let server = make_server(project_root, data_dir.clone());
    let bootstrap = bootstrap(&server)?;

    let store_path =
        harness_core::config::dirs::default_db_path(&bootstrap.data_dir, "runtime_state");
    std::fs::create_dir_all(&store_path)?;

    let runtime = restore_runtime_state(&bootstrap).await?;

    assert!(runtime.runtime_state_store.is_none());
    assert!(runtime.runtime_hosts.snapshot_state().0.is_empty());
    assert!(runtime.runtime_project_cache.snapshot_state().is_empty());
    Ok(())
}

#[tokio::test]
async fn restore_runtime_state_restores_snapshot_contents() -> anyhow::Result<()> {
    let sandbox = tempfile::tempdir()?;
    let project_root = sandbox.path().join("project");
    std::fs::create_dir_all(&project_root)?;
    let data_dir = sandbox.path().join("data");
    let server = make_server(project_root, data_dir.clone());
    let bootstrap = bootstrap(&server)?;

    let store_path =
        harness_core::config::dirs::default_db_path(&bootstrap.data_dir, "runtime_state");
    let store = crate::runtime_state_store::RuntimeStateStore::open(&store_path).await?;
    store.persist_snapshot(vec![], vec![], vec![]).await?;

    let runtime = restore_runtime_state(&bootstrap).await?;

    assert!(runtime.runtime_state_store.is_some());
    assert!(runtime.runtime_hosts.snapshot_state().0.is_empty());
    assert!(runtime.runtime_project_cache.snapshot_state().is_empty());
    Ok(())
}
