use dashmap::DashMap;
use std::path::Path;
use std::sync::Arc;

use crate::server::HarnessServer;

/// Outputs of the registry initialization phase.
pub(crate) struct RegistryBundle {
    pub thread_db: crate::thread_db::ThreadDb,
    pub plan_db: crate::plan_db::PlanDb,
    pub plan_cache: Arc<DashMap<String, harness_exec::plan::ExecPlan>>,
    pub project_registry: Arc<crate::project_registry::ProjectRegistry>,
    pub runtime_state_store: Option<Arc<crate::runtime_state_store::RuntimeStateStore>>,
    pub workspace_mgr: Option<Arc<crate::workspace::WorkspaceManager>>,
}

/// Initialize thread DB, plan DB, plan cache, project registry, workspace
/// manager, and runtime state store.
///
/// Must follow `build_storage` (uses `storage.tasks` for orphan cleanup) —
/// callers must pass `tasks` explicitly for that cleanup step.
pub(crate) async fn build_registry(
    server: &Arc<HarnessServer>,
    data_dir: &Path,
    project_root: &Path,
    tasks: &Arc<crate::task_runner::TaskStore>,
) -> anyhow::Result<RegistryBundle> {
    // ── Thread DB ─────────────────────────────────────────────────────────────
    let thread_db_path = harness_core::config::dirs::default_db_path(data_dir, "threads");
    let thread_db = crate::thread_db::ThreadDb::open(&thread_db_path).await?;

    // Load persisted threads into the in-memory ThreadManager cache.
    for thread in thread_db.list().await? {
        server
            .thread_manager
            .threads_cache()
            .insert(thread.id.as_str().to_string(), thread);
    }

    // ── Plan DB + cache ───────────────────────────────────────────────────────
    let plan_db = crate::plan_db::PlanDb::open(&harness_core::config::dirs::default_db_path(
        data_dir, "plans",
    ))
    .await?;

    let plans_md_dir = data_dir.join("plans");
    match plan_db.migrate_from_markdown_dir(&plans_md_dir).await {
        Ok(0) => {}
        Ok(n) => tracing::debug!(
            count = n,
            "plan migration: imported {} plan(s) from markdown",
            n
        ),
        Err(e) => tracing::warn!("plan migration: failed: {e}"),
    }

    let plan_cache: Arc<DashMap<String, harness_exec::plan::ExecPlan>> = Arc::new(DashMap::new());
    match plan_db.list().await {
        Ok(plans) => {
            let count = plans.len();
            for plan in plans {
                plan_cache.insert(plan.id.as_str().to_string(), plan);
            }
            if count > 0 {
                tracing::debug!(count, "plan cache: loaded {} plan(s) from db", count);
            }
        }
        Err(e) => tracing::warn!("plan cache: failed to load plans on startup: {e}"),
    }

    // ── Project registry ──────────────────────────────────────────────────────
    let project_registry = crate::project_registry::ProjectRegistry::open(
        &harness_core::config::dirs::default_db_path(data_dir, "projects"),
    )
    .await?;

    // Auto-register the default project from --project-root on startup.
    let canonical_project_root = project_root.canonicalize().ok();
    let project_matches_root = |project: &harness_core::config::ProjectEntry| {
        canonical_project_root
            .as_ref()
            .and_then(|canonical_root| {
                project
                    .root
                    .canonicalize()
                    .ok()
                    .map(|root| root == *canonical_root)
            })
            .unwrap_or_else(|| project.root == project_root)
    };
    let default_project_metadata = server
        .startup_default_project
        .as_ref()
        .filter(|project| project_matches_root(project))
        .or_else(|| {
            server
                .startup_projects
                .iter()
                .find(|project| project.default && project_matches_root(project))
                .or_else(|| {
                    server
                        .startup_projects
                        .iter()
                        .find(|project| project_matches_root(project))
                })
        });
    let default_project = crate::project_registry::Project {
        id: harness_core::types::ProjectId::from_path(project_root)
            .as_str()
            .to_owned(),
        root: project_root.to_path_buf(),
        max_concurrent: default_project_metadata.and_then(|p| p.max_concurrent),
        default_agent: default_project_metadata.and_then(|p| p.default_agent.clone()),
        active: true,
        created_at: chrono::Utc::now().to_rfc3339(),
    };
    if let Err(e) = project_registry.register(default_project).await {
        tracing::warn!("failed to auto-register default project: {e}");
    }
    for project in &server.startup_projects {
        let proj = crate::project_registry::Project {
            id: harness_core::types::ProjectId::from_path(&project.root)
                .as_str()
                .to_owned(),
            root: project.root.clone(),
            max_concurrent: project.max_concurrent,
            default_agent: project.default_agent.clone(),
            active: true,
            created_at: chrono::Utc::now().to_rfc3339(),
        };
        if let Err(e) = project_registry.register(proj).await {
            tracing::warn!(project = %project.name, "failed to register startup project: {e}");
        }
    }

    // ── Workspace manager ─────────────────────────────────────────────────────
    let workspace_mgr =
        match crate::workspace::WorkspaceManager::new(server.config.workspace.clone()) {
            Ok(mgr) => {
                tracing::debug!(
                    root = %server.config.workspace.root.display(),
                    "workspace manager initialized"
                );
                Some(Arc::new(mgr))
            }
            Err(e) => {
                tracing::warn!(
                "failed to initialize workspace manager: {e}; running without workspace isolation"
            );
                None
            }
        };

    // Cleanup orphan worktrees from any previous crash.
    // Terminal tasks are no longer held in the in-memory cache, so query DB directly.
    if let Some(ref wmgr) = workspace_mgr {
        match tasks.list_terminal_ids_from_db().await {
            Ok(terminal_ids) => {
                wmgr.cleanup_orphan_worktrees(project_root, &terminal_ids)
                    .await;
            }
            Err(e) => {
                tracing::warn!(
                    "Failed to load terminal tasks for orphan worktree cleanup: {e}; skipping cleanup"
                );
            }
        }
    }

    // ── Runtime state store ───────────────────────────────────────────────────
    let runtime_state_store = {
        let runtime_state_db_path =
            harness_core::config::dirs::default_db_path(data_dir, "runtime_state");
        match crate::runtime_state_store::RuntimeStateStore::open(&runtime_state_db_path).await {
            Ok(store) => Some(Arc::new(store)),
            Err(e) => {
                tracing::warn!(
                    path = %runtime_state_db_path.display(),
                    "runtime state store init failed, runtime host state will not persist: {e}"
                );
                None
            }
        }
    };

    Ok(RegistryBundle {
        thread_db,
        plan_db,
        plan_cache,
        project_registry,
        runtime_state_store,
        workspace_mgr,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{server::HarnessServer, thread_manager::ThreadManager};
    use harness_agents::registry::AgentRegistry;
    use harness_core::config::HarnessConfig;

    async fn make_test_server_and_tasks(
        dir: &Path,
    ) -> (Arc<HarnessServer>, Arc<crate::task_runner::TaskStore>) {
        let server = Arc::new(HarnessServer::new(
            HarnessConfig::default(),
            ThreadManager::new(),
            AgentRegistry::new("test"),
        ));
        let tasks = crate::task_runner::TaskStore::open(
            &harness_core::config::dirs::default_db_path(dir, "tasks"),
        )
        .await
        .expect("open tasks db");
        (server, tasks)
    }

    #[tokio::test]
    async fn empty_data_dir_produces_empty_project_registry() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (server, tasks) = make_test_server_and_tasks(dir.path()).await;
        let bundle = build_registry(&server, dir.path(), dir.path(), &tasks)
            .await
            .expect("build_registry should succeed");
        // The default project is always auto-registered; registry should have exactly 1 entry.
        let projects = bundle.project_registry.list().await.expect("list projects");
        assert_eq!(projects.len(), 1, "expected only the default project");
        // ID is the canonical path of the project root, not the literal "default".
        let canonical = dir
            .path()
            .canonicalize()
            .unwrap_or_else(|_| dir.path().to_path_buf());
        assert_eq!(projects[0].id, canonical.to_string_lossy().as_ref());
    }

    #[tokio::test]
    async fn plan_cache_hydrated_from_db() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (server, tasks) = make_test_server_and_tasks(dir.path()).await;

        // Pre-insert a plan directly into the DB before calling build_registry.
        let plan_db = crate::plan_db::PlanDb::open(&harness_core::config::dirs::default_db_path(
            dir.path(),
            "plans",
        ))
        .await
        .expect("open plan db");
        let plan = harness_exec::plan::ExecPlan::from_spec("# test-plan-id", dir.path())
            .expect("build plan");
        plan_db.upsert(&plan).await.expect("upsert plan");
        let plan_id = plan.id.as_str().to_string();

        let bundle = build_registry(&server, dir.path(), dir.path(), &tasks)
            .await
            .expect("build_registry");
        assert!(
            bundle.plan_cache.contains_key(&plan_id),
            "plan should be hydrated into cache"
        );
    }
}
