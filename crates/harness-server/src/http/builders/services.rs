use std::path::Path;
use std::sync::Arc;

use crate::server::HarnessServer;

use super::{
    engines::EnginesBundle, intake::IntakeBundle, registry::RegistryBundle, storage::StorageBundle,
};

/// Outputs of the service layer initialization phase.
pub(crate) struct ServicesBundle {
    pub interceptors: Vec<Arc<dyn harness_core::interceptor::TurnInterceptor>>,
    pub project_svc: Arc<dyn crate::services::project::ProjectService>,
    pub task_svc: Arc<dyn crate::services::task::TaskService>,
    pub execution_svc: Arc<dyn crate::services::execution::ExecutionService>,
    pub runtime_hosts: Arc<crate::runtime_hosts::RuntimeHostManager>,
    pub runtime_project_cache: Arc<crate::runtime_project_cache::RuntimeProjectCacheManager>,
}

/// Initialize interceptors, service impls, runtime host/project-cache managers,
/// and restore any persisted runtime state snapshot.
///
/// Also spawns the background task-recovery validator.
///
/// Depends on: `storage`, `engines`, `registry`, `intake` — must follow all four.
pub(crate) async fn build_services(
    server: &Arc<HarnessServer>,
    storage: &StorageBundle,
    engines: &EnginesBundle,
    registry: &RegistryBundle,
    intake: &IntakeBundle,
    project_root: &Path,
) -> anyhow::Result<ServicesBundle> {
    // ── Interceptor stack ─────────────────────────────────────────────────────
    let hook_enforcement = server.config.rules.hook_enforcement;
    let interceptors: Vec<Arc<dyn harness_core::interceptor::TurnInterceptor>> = vec![
        Arc::new(crate::contract_validator::ContractValidator::new()),
        // RuleEnforcer disabled: false-positives on test fixtures in other repo
        // worktrees (SEC-02 hardcoded secrets in test code) block periodic_review
        // tasks. Re-enable after adding source-aware filtering or allowlists.
        // Arc::new(crate::rule_enforcer::RuleEnforcer::new(
        //     engines.rules.clone(),
        // )),
        Arc::new(crate::hook_enforcer::HookEnforcer::new(
            engines.rules.clone(),
            engines.events.clone(),
            hook_enforcement,
        )),
        Arc::new(crate::post_validator::PostExecutionValidator::new(
            server.config.validation.clone(),
        )),
    ];

    // ── Service layer ─────────────────────────────────────────────────────────
    let project_svc = crate::services::project::DefaultProjectService::new(
        registry.project_registry.clone(),
        project_root.to_path_buf(),
    );
    let task_svc = crate::services::task::DefaultTaskService::new(storage.tasks.clone());
    let execution_svc = crate::services::execution::DefaultExecutionService::new(
        storage.tasks.clone(),
        server.agent_registry.clone(),
        Arc::new(server.config.clone()),
        engines.skills.clone(),
        engines.events.clone(),
        interceptors.clone(),
        registry.workspace_mgr.clone(),
        intake.task_queue.clone(),
        intake.completion_callback.clone(),
        Some(registry.project_registry.clone()),
        server.config.server.allowed_project_roots.clone(),
    );

    // ── Runtime hosts + project cache ─────────────────────────────────────────
    let runtime_hosts = Arc::new(crate::runtime_hosts::RuntimeHostManager::new());
    let runtime_project_cache =
        Arc::new(crate::runtime_project_cache::RuntimeProjectCacheManager::new());

    // Restore persisted runtime state snapshot when available.
    if let Some(store) = registry.runtime_state_store.as_ref() {
        match store.try_load_snapshot().await {
            Ok((Some(snapshot), crate::runtime_state_store::LoadSnapshotOutcome::Loaded)) => {
                let restored_hosts = runtime_hosts.restore_state(snapshot.hosts, snapshot.leases);
                let restored_project_caches =
                    runtime_project_cache.restore_state(snapshot.project_caches);
                tracing::info!(
                    restored_hosts = restored_hosts.0,
                    restored_leases = restored_hosts.1,
                    restored_project_caches,
                    "runtime state restored from persistent snapshot"
                );
            }
            Ok((None, crate::runtime_state_store::LoadSnapshotOutcome::NotFound)) => {
                tracing::info!("no runtime state snapshot found on startup");
            }
            Ok((
                None,
                crate::runtime_state_store::LoadSnapshotOutcome::SchemaMismatch { found, expected },
            )) => {
                tracing::warn!(
                    found_schema_version = found,
                    expected_schema_version = expected,
                    "runtime state snapshot skipped on startup due to schema mismatch"
                );
            }
            Ok((None, crate::runtime_state_store::LoadSnapshotOutcome::Loaded)) => {
                tracing::warn!("runtime state snapshot load returned loaded outcome without data");
            }
            Ok((Some(_), outcome)) => {
                tracing::warn!(
                    ?outcome,
                    "runtime state snapshot load returned unexpected outcome"
                );
            }
            Err(e) => {
                tracing::warn!("failed to load runtime state snapshot on startup: {e}");
            }
        }
    }

    // Spawn background validator for recovered pending tasks.
    // The completion callback is passed so that tasks marked Failed (closed PR)
    // trigger intake cleanup (e.g. removing the issue from the dispatched map).
    {
        let tasks_for_recovery = storage.tasks.clone();
        let cb_for_recovery = intake.completion_callback.clone();
        tokio::spawn(async move {
            tasks_for_recovery
                .validate_recovered_tasks(cb_for_recovery)
                .await;
            // Sweep at startup then every 5 minutes so tasks that complete during
            // this server session get pr_state populated without waiting for a restart.
            loop {
                tasks_for_recovery.validate_done_tasks_pr_state().await;
                tokio::time::sleep(std::time::Duration::from_secs(300)).await;
            }
        });
    }

    Ok(ServicesBundle {
        interceptors,
        project_svc,
        task_svc,
        execution_svc,
        runtime_hosts,
        runtime_project_cache,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{server::HarnessServer, thread_manager::ThreadManager};
    use harness_agents::registry::AgentRegistry;
    use harness_core::config::HarnessConfig;

    async fn make_all_bundles(
        dir: &std::path::Path,
    ) -> (
        Arc<HarnessServer>,
        StorageBundle,
        EnginesBundle,
        RegistryBundle,
        IntakeBundle,
    ) {
        let server = Arc::new(HarnessServer::new(
            HarnessConfig::default(),
            ThreadManager::new(),
            AgentRegistry::new("test"),
        ));
        let storage = crate::http::builders::storage::build_storage(dir)
            .await
            .expect("storage");
        let engines = crate::http::builders::engines::build_engines(&server, dir, dir)
            .await
            .expect("engines");
        let registry =
            crate::http::builders::registry::build_registry(&server, dir, dir, &storage.tasks)
                .await
                .expect("registry");
        let intake = crate::http::builders::intake::build_intake(
            &server, &storage, &engines, &registry, dir, dir,
        )
        .await
        .expect("intake");
        (server, storage, engines, registry, intake)
    }

    #[tokio::test]
    async fn interceptor_count_matches_registered() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (server, storage, engines, registry, intake) = make_all_bundles(dir.path()).await;
        let bundle = build_services(&server, &storage, &engines, &registry, &intake, dir.path())
            .await
            .expect("build_services");
        // 3 interceptors: ContractValidator, HookEnforcer, PostExecutionValidator
        // (RuleEnforcer temporarily disabled due to false-positive blocks)
        assert_eq!(bundle.interceptors.len(), 3, "expected 3 interceptors");
    }
}
