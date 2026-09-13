use std::path::Path;
use std::sync::Arc;

use crate::server::HarnessServer;

use super::{registry::RegistryBundle, storage::StorageBundle};

/// Outputs of the service layer initialization phase.
pub(crate) struct ServicesBundle {
    pub project_svc: Arc<dyn crate::services::project::ProjectService>,
    pub task_svc: Arc<dyn crate::services::task::TaskService>,
    pub execution_svc: Arc<dyn crate::services::execution::ExecutionService>,
    pub runtime_hosts: Arc<crate::runtime_hosts::RuntimeHostManager>,
    pub runtime_project_cache: Arc<crate::runtime_project_cache::RuntimeProjectCacheManager>,
    /// True when the runtime state snapshot load was attempted but failed
    /// (schema mismatch, I/O error, or unexpected outcome). The store itself
    /// may still be usable for future writes; only the recovery was incomplete.
    pub snapshot_load_failed: bool,
}

/// Initialize service impls, runtime host/project-cache managers,
/// and restore any persisted runtime state snapshot.
///
/// Also spawns the background task-recovery validator.
///
/// Depends on initialized storage and registry services.
pub(crate) async fn build_services(
    server: &Arc<HarnessServer>,
    storage: &StorageBundle,
    registry: &RegistryBundle,
    project_root: &Path,
) -> anyhow::Result<ServicesBundle> {
    let project_registry = registry
        .project_registry
        .as_ref()
        .expect("build_services requires a ready project registry")
        .clone();

    // ── Service layer ─────────────────────────────────────────────────────────
    let project_svc = crate::services::project::DefaultProjectService::new(
        project_registry.clone(),
        project_root.to_path_buf(),
    );
    let task_svc: Arc<dyn crate::services::task::TaskService> = match storage.tasks.as_ref() {
        Some(tasks) => crate::services::task::DefaultTaskService::new(tasks.clone()),
        None => crate::services::task::UnavailableTaskService::new(),
    };
    let execution_svc = crate::services::execution::DefaultExecutionService::new(
        Arc::new(server.config.clone()),
        registry.workflow_runtime_store.clone(),
        Some(project_registry.clone()),
        server.config.server.allowed_project_roots.clone(),
    );

    // ── Runtime hosts + project cache ─────────────────────────────────────────
    let runtime_hosts = Arc::new(crate::runtime_hosts::RuntimeHostManager::new());
    let runtime_project_cache =
        Arc::new(crate::runtime_project_cache::RuntimeProjectCacheManager::new());

    // Restore persisted runtime state snapshot when available.
    let mut snapshot_load_failed = false;
    if let Some(store) = registry.runtime_state_store.as_ref() {
        match store.try_load_snapshot().await {
            Ok((Some(snapshot), crate::runtime_state_store::LoadSnapshotOutcome::Loaded)) => {
                let restored_hosts = runtime_hosts.restore_state(snapshot.hosts);
                let restored_project_caches =
                    runtime_project_cache.restore_state(snapshot.project_caches);
                tracing::info!(
                    restored_hosts,
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
                snapshot_load_failed = true;
            }
            Ok((None, crate::runtime_state_store::LoadSnapshotOutcome::Loaded)) => {
                tracing::warn!("runtime state snapshot load returned loaded outcome without data");
                snapshot_load_failed = true;
            }
            Ok((Some(_), outcome)) => {
                tracing::warn!(
                    ?outcome,
                    "runtime state snapshot load returned unexpected outcome"
                );
                snapshot_load_failed = true;
            }
            Err(e) => {
                tracing::warn!("failed to load runtime state snapshot on startup: {e}");
                snapshot_load_failed = true;
            }
        }
    }

    Ok(ServicesBundle {
        project_svc,
        task_svc,
        execution_svc,
        runtime_hosts,
        runtime_project_cache,
        snapshot_load_failed,
    })
}
