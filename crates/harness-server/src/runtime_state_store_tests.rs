use crate::runtime_project_cache::WatchedProjectInput;
use crate::{http::build_app_state, server::HarnessServer, thread_manager::ThreadManager};
use harness_agents::registry::AgentRegistry;
use harness_core::config::HarnessConfig;
use std::sync::Arc;

#[tokio::test]
async fn build_app_state_restores_runtime_snapshot() -> anyhow::Result<()> {
    let _lock = crate::test_helpers::HOME_LOCK.lock().await;
    let project_root = crate::test_helpers::tempdir_in_home("runtime-state-root-")?;
    let data_dir = tempfile::tempdir()?;

    let mut config = HarnessConfig::default();
    config.server.project_root = project_root.path().to_path_buf();
    config.server.data_dir = data_dir.path().to_path_buf();

    let first_server = Arc::new(HarnessServer::new(
        config.clone(),
        ThreadManager::new(),
        AgentRegistry::new("test"),
    ));
    let first_state = build_app_state(first_server).await?;

    first_state.runtime_hosts.register(
        "host-a".to_string(),
        Some("Host A".to_string()),
        vec!["codex".to_string()],
    );
    first_state.runtime_project_cache.sync_host_projects(
        "host-a",
        vec![WatchedProjectInput {
            project_id: Some("default".to_string()),
            root: project_root
                .path()
                .canonicalize()?
                .to_string_lossy()
                .into_owned(),
        }],
    );
    first_state.persist_runtime_state().await?;
    drop(first_state);

    let second_server = Arc::new(HarnessServer::new(
        config,
        ThreadManager::new(),
        AgentRegistry::new("test"),
    ));
    let restored_state = build_app_state(second_server).await?;

    let hosts = restored_state.runtime_hosts.list_hosts();
    assert_eq!(hosts.len(), 1);
    assert_eq!(hosts[0].id, "host-a");

    let project_cache = restored_state
        .runtime_project_cache
        .get_host_cache("host-a")
        .expect("runtime project cache for host-a should be restored");
    assert_eq!(project_cache.project_count, 1);
    assert_eq!(
        project_cache.projects[0].project_id.as_deref(),
        Some("default")
    );
    Ok(())
}
