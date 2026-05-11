use dashmap::DashMap;
use std::path::Path;
use std::sync::Arc;

use super::registry_migration::{
    migrate_issue_workflows_if_needed, migrate_project_workflows_if_needed,
    repair_corrupt_project_ids,
};
use crate::http::state::StoreStartupResult;
use crate::server::HarnessServer;

/// Outputs of the registry initialization phase.
pub(crate) struct RegistryBundle {
    pub thread_db: Option<crate::thread_db::ThreadDb>,
    pub plan_db: Option<crate::plan_db::PlanDb>,
    pub plan_cache: Arc<DashMap<String, harness_exec::plan::ExecPlan>>,
    pub issue_workflow_store: Option<Arc<harness_workflow::issue_lifecycle::IssueWorkflowStore>>,
    pub project_workflow_store:
        Option<Arc<harness_workflow::project_lifecycle::ProjectWorkflowStore>>,
    pub workflow_runtime_store: Option<Arc<harness_workflow::runtime::WorkflowRuntimeStore>>,
    pub project_registry: Option<Arc<crate::project_registry::ProjectRegistry>>,
    pub runtime_state_store: Option<Arc<crate::runtime_state_store::RuntimeStateStore>>,
    pub workspace_mgr: Option<Arc<crate::workspace::WorkspaceManager>>,
    pub startup_results: Vec<StoreStartupResult>,
}

fn failed_registry_startup_results(error: &str) -> Vec<StoreStartupResult> {
    vec![
        StoreStartupResult::critical("thread_db").failed(error),
        StoreStartupResult::critical("plan_db").failed(error),
        StoreStartupResult::optional("issue_workflow_store").failed(error),
        StoreStartupResult::optional("project_workflow_store").failed(error),
        StoreStartupResult::optional("workflow_runtime_store").failed(error),
        StoreStartupResult::critical("project_registry").failed(error),
        StoreStartupResult::optional("workspace_manager").failed(error),
        StoreStartupResult::optional("runtime_state_store").failed(error),
    ]
}

fn failed_registry_bundle(
    plan_cache: Arc<DashMap<String, harness_exec::plan::ExecPlan>>,
    error: &str,
) -> RegistryBundle {
    RegistryBundle {
        thread_db: None,
        plan_db: None,
        plan_cache,
        issue_workflow_store: None,
        project_workflow_store: None,
        workflow_runtime_store: None,
        project_registry: None,
        runtime_state_store: None,
        workspace_mgr: None,
        startup_results: failed_registry_startup_results(error),
    }
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
    let configured_database_url = server.config.server.database_url.as_deref();
    let workflow_config =
        harness_core::config::workflow::load_workflow_config(project_root).unwrap_or_default();
    let workflow_ns = workflow_config.storage.schema_namespace;
    let mut startup_results = Vec::new();
    let plan_cache: Arc<DashMap<String, harness_exec::plan::ExecPlan>> = Arc::new(DashMap::new());

    let database_url = match harness_core::db::resolve_database_url(configured_database_url) {
        Ok(database_url) => database_url,
        Err(error) => {
            let error = error.to_string();
            return Ok(failed_registry_bundle(plan_cache, &error));
        }
    };
    let setup_pool = match harness_core::db::pg_open_pool(&database_url).await {
        Ok(pool) => pool,
        Err(error) => {
            let error = error.to_string();
            return Ok(failed_registry_bundle(plan_cache, &error));
        }
    };

    // ── Thread DB ─────────────────────────────────────────────────────────────
    let thread_db_path = harness_core::config::dirs::default_db_path(data_dir, "threads");
    let thread_context = harness_core::db::PgStoreContext::new(
        database_url.clone(),
        harness_core::db::pg_schema_for_path(&thread_db_path)?,
    )?;
    let thread_db = match super::forced_startup_error("thread_db") {
        Some(error) => {
            startup_results.push(StoreStartupResult::critical("thread_db").failed(error));
            None
        }
        None => match crate::thread_db::ThreadDb::open_with_context(&thread_context, &setup_pool)
            .await
        {
            Ok(thread_db) => Some(thread_db),
            Err(error) => {
                startup_results
                    .push(StoreStartupResult::critical("thread_db").failed(error.to_string()));
                None
            }
        },
    };

    // Load persisted threads into the in-memory ThreadManager cache.
    if let Some(thread_db) = thread_db.as_ref() {
        match thread_db.list().await {
            Ok(threads) => {
                for thread in threads {
                    server
                        .thread_manager
                        .threads_cache()
                        .insert(thread.id.as_str().to_string(), thread);
                }
                startup_results.push(StoreStartupResult::critical("thread_db"));
            }
            Err(error) => {
                startup_results
                    .push(StoreStartupResult::critical("thread_db").failed(error.to_string()));
                tracing::warn!("thread cache: failed to load threads on startup: {error}");
            }
        }
    }

    // ── Plan DB + cache ───────────────────────────────────────────────────────
    let plans_db_path = harness_core::config::dirs::default_db_path(data_dir, "plans");
    let plan_context = harness_core::db::PgStoreContext::new(
        database_url.clone(),
        harness_core::db::pg_schema_for_path(&plans_db_path)?,
    )?;
    let plan_db = match super::forced_startup_error("plan_db") {
        Some(error) => {
            startup_results.push(StoreStartupResult::critical("plan_db").failed(error));
            None
        }
        None => match crate::plan_db::PlanDb::open_with_context(&plan_context, &setup_pool).await {
            Ok(plan_db) => {
                startup_results.push(StoreStartupResult::critical("plan_db"));
                Some(plan_db)
            }
            Err(error) => {
                startup_results
                    .push(StoreStartupResult::critical("plan_db").failed(error.to_string()));
                None
            }
        },
    };

    let plans_md_dir = data_dir.join("plans");
    if let Some(plan_db) = plan_db.as_ref() {
        match plan_db.migrate_from_markdown_dir(&plans_md_dir).await {
            Ok(0) => {}
            Ok(n) => tracing::debug!(
                count = n,
                "plan migration: imported {} plan(s) from markdown",
                n
            ),
            Err(e) => tracing::warn!("plan migration: failed: {e}"),
        }

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
    }

    // ── Issue workflow store ──────────────────────────────────────────────────
    let issue_workflows_db_path =
        harness_core::config::dirs::default_db_path(data_dir, "issue_workflows");
    let issue_workflow_store = {
        let schema = format!("{workflow_ns}_issue");
        match super::forced_startup_error("issue_workflow_store") {
            Some(error) => {
                startup_results
                    .push(StoreStartupResult::optional("issue_workflow_store").failed(error));
                None
            }
            None => match harness_core::db::PgStoreContext::new(
                database_url.clone(),
                schema.clone(),
            ) {
                Ok(context) => {
                    match harness_workflow::issue_lifecycle::IssueWorkflowStore::open_with_context(
                        &context,
                        &setup_pool,
                    )
                    .await
                    {
                        Ok(store) => {
                            startup_results
                                .push(StoreStartupResult::optional("issue_workflow_store"));
                            if let Err(e) = migrate_issue_workflows_if_needed(
                                configured_database_url,
                                &issue_workflows_db_path,
                                &schema,
                                &store,
                            )
                            .await
                            {
                                tracing::warn!(
                                    schema = %schema,
                                    "issue workflow migration check failed: {e}"
                                );
                            }
                            let (rewritten, failed, skipped) =
                                repair_corrupt_project_ids(&store, project_root).await;
                            tracing::info!(
                                rewritten,
                                failed,
                                skipped,
                                "startup repair of corrupt issue workflow project_ids complete"
                            );
                            Some(Arc::new(store))
                        }
                        Err(e) => {
                            startup_results.push(
                                StoreStartupResult::optional("issue_workflow_store")
                                    .failed(e.to_string()),
                            );
                            tracing::warn!(
                                schema = %schema,
                                "issue workflow store init failed, issue lifecycle state will not persist: {e}"
                            );
                            None
                        }
                    }
                }
                Err(error) => {
                    startup_results.push(
                        StoreStartupResult::optional("issue_workflow_store")
                            .failed(error.to_string()),
                    );
                    tracing::warn!(
                        schema = %schema,
                        "issue workflow store context init failed, issue lifecycle state will not persist: {error}"
                    );
                    None
                }
            },
        }
    };

    // ── Project workflow store ────────────────────────────────────────────────
    let project_workflows_db_path =
        harness_core::config::dirs::default_db_path(data_dir, "project_workflows");
    let project_workflow_store = {
        let schema = format!("{workflow_ns}_project");
        match super::forced_startup_error("project_workflow_store") {
            Some(error) => {
                startup_results
                    .push(StoreStartupResult::optional("project_workflow_store").failed(error));
                None
            }
            None => match harness_core::db::PgStoreContext::new(
                database_url.clone(),
                schema.clone(),
            ) {
                Ok(context) => {
                    match harness_workflow::project_lifecycle::ProjectWorkflowStore::open_with_context(
                        &context,
                        &setup_pool,
                    )
                    .await
                    {
                        Ok(store) => {
                            startup_results
                                .push(StoreStartupResult::optional("project_workflow_store"));
                            if let Err(e) = migrate_project_workflows_if_needed(
                                configured_database_url,
                                &project_workflows_db_path,
                                &schema,
                                &store,
                            )
                            .await
                            {
                                tracing::warn!(
                                    schema = %schema,
                                    "project workflow migration check failed: {e}"
                                );
                            }
                            Some(Arc::new(store))
                        }
                        Err(e) => {
                            startup_results.push(
                                StoreStartupResult::optional("project_workflow_store")
                                    .failed(e.to_string()),
                            );
                            tracing::warn!(
                                schema = %schema,
                                "project workflow store init failed, project lifecycle state will not persist: {e}"
                            );
                            None
                        }
                    }
                }
                Err(error) => {
                    startup_results.push(
                        StoreStartupResult::optional("project_workflow_store")
                            .failed(error.to_string()),
                    );
                    tracing::warn!(
                        schema = %schema,
                        "project workflow store context init failed, project lifecycle state will not persist: {error}"
                    );
                    None
                }
            },
        }
    };

    // ── Project registry ──────────────────────────────────────────────────────
    let workflow_runtime_store = {
        let schema = format!("{workflow_ns}_runtime");
        match super::forced_startup_error("workflow_runtime_store") {
            Some(error) => {
                startup_results
                    .push(StoreStartupResult::optional("workflow_runtime_store").failed(error));
                None
            }
            None => {
                match harness_core::db::PgStoreContext::new(database_url.clone(), schema.clone()) {
                    Ok(context) => {
                        match harness_workflow::runtime::WorkflowRuntimeStore::open_with_context(
                            &context,
                            &setup_pool,
                        )
                        .await
                        {
                            Ok(store) => {
                                startup_results
                                    .push(StoreStartupResult::optional("workflow_runtime_store"));
                                Some(Arc::new(store))
                            }
                            Err(e) => {
                                startup_results.push(
                                    StoreStartupResult::optional("workflow_runtime_store")
                                        .failed(e.to_string()),
                                );
                                tracing::warn!(
                                    schema = %schema,
                                    "workflow runtime store init failed, generic workflow decisions will not persist: {e}"
                                );
                                None
                            }
                        }
                    }
                    Err(error) => {
                        startup_results.push(
                            StoreStartupResult::optional("workflow_runtime_store")
                                .failed(error.to_string()),
                        );
                        tracing::warn!(
                            schema = %schema,
                            "workflow runtime store context init failed, generic workflow decisions will not persist: {error}"
                        );
                        None
                    }
                }
            }
        }
    };

    // ── Project registry ──────────────────────────────────────────────────────
    let projects_db_path = harness_core::config::dirs::default_db_path(data_dir, "projects");
    let project_context = harness_core::db::PgStoreContext::new(
        database_url.clone(),
        harness_core::db::pg_schema_for_path(&projects_db_path)?,
    )?;
    let project_registry = match super::forced_startup_error("project_registry") {
        Some(error) => {
            startup_results.push(StoreStartupResult::critical("project_registry").failed(error));
            None
        }
        None => match crate::project_registry::ProjectRegistry::open_with_context(
            &project_context,
            &setup_pool,
        )
        .await
        {
            Ok(project_registry) => {
                startup_results.push(StoreStartupResult::critical("project_registry"));
                Some(project_registry)
            }
            Err(error) => {
                startup_results.push(
                    StoreStartupResult::critical("project_registry").failed(error.to_string()),
                );
                None
            }
        },
    };

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
    let canonical_root = canonical_project_root.as_deref().unwrap_or(project_root);
    let default_project = crate::project_registry::Project {
        id: harness_core::types::ProjectId::from_path(canonical_root)
            .as_str()
            .to_owned(),
        root: canonical_root.to_path_buf(),
        name: default_project_metadata.map(|p| p.name.clone()),
        max_concurrent: default_project_metadata.and_then(|p| p.max_concurrent),
        default_agent: default_project_metadata.and_then(|p| p.default_agent.clone()),
        active: true,
        created_at: chrono::Utc::now().to_rfc3339(),
    };
    if let Some(project_registry) = project_registry.as_ref() {
        if let Err(e) = project_registry.register(default_project).await {
            tracing::warn!("failed to auto-register default project: {e}");
        }
        for project in &server.startup_projects {
            let startup_root = project
                .root
                .canonicalize()
                .unwrap_or_else(|_| project.root.clone());
            let proj = crate::project_registry::Project {
                id: harness_core::types::ProjectId::from_path(&startup_root)
                    .as_str()
                    .to_owned(),
                root: startup_root,
                name: Some(project.name.clone()),
                max_concurrent: project.max_concurrent,
                default_agent: project.default_agent.clone(),
                active: true,
                created_at: chrono::Utc::now().to_rfc3339(),
            };
            if let Err(e) = project_registry.register(proj).await {
                tracing::warn!(project = %project.name, "failed to register startup project: {e}");
            }
        }
    }

    // ── Workspace manager ─────────────────────────────────────────────────────
    let workspace_mgr =
        match crate::workspace::WorkspaceManager::new(server.config.workspace.clone()) {
            Ok(mgr) => {
                startup_results.push(StoreStartupResult::optional("workspace_manager"));
                tracing::debug!(
                    root = %server.config.workspace.root.display(),
                    "workspace manager initialized"
                );
                Some(Arc::new(mgr))
            }
            Err(e) => {
                startup_results
                    .push(StoreStartupResult::optional("workspace_manager").failed(e.to_string()));
                tracing::warn!(
                "failed to initialize workspace manager: {e}; running without workspace isolation"
            );
                None
            }
        };

    // Reconcile stale workspaces from any previous crash before new task admission.
    if let Some(ref wmgr) = workspace_mgr {
        match tasks.list_all_summaries_with_terminal().await {
            Ok(task_summaries) => {
                match wmgr.reconcile_startup(project_root, &task_summaries).await {
                    Ok(summary) => {
                        tracing::info!(
                            removed = summary.removed,
                            preserved = summary.preserved,
                            migrated = summary.migrated,
                            "workspace startup reconciliation complete"
                        );
                    }
                    Err(e) => {
                        tracing::warn!("workspace startup reconciliation failed: {e}");
                    }
                }
            }
            Err(e) => {
                tracing::warn!(
                    "Failed to load task summaries for workspace reconciliation: {e}; skipping cleanup"
                );
            }
        }
    }

    // ── Runtime state store ───────────────────────────────────────────────────
    let runtime_state_store = {
        let runtime_state_db_path =
            harness_core::config::dirs::default_db_path(data_dir, "runtime_state");
        match super::forced_startup_error("runtime_state_store") {
            Some(error) => {
                startup_results
                    .push(StoreStartupResult::optional("runtime_state_store").failed(error));
                None
            }
            None => match harness_core::db::pg_schema_for_path(&runtime_state_db_path).and_then(
                |schema| harness_core::db::PgStoreContext::new(database_url.clone(), schema),
            ) {
                Ok(context) => {
                    match crate::runtime_state_store::RuntimeStateStore::open_with_context(
                        &context,
                        &setup_pool,
                    )
                    .await
                    {
                        Ok(store) => {
                            startup_results
                                .push(StoreStartupResult::optional("runtime_state_store"));
                            Some(Arc::new(store))
                        }
                        Err(e) => {
                            startup_results.push(
                                StoreStartupResult::optional("runtime_state_store")
                                    .failed(e.to_string()),
                            );
                            tracing::warn!(
                                path = %runtime_state_db_path.display(),
                                "runtime state store init failed, runtime host state will not persist: {e}"
                            );
                            None
                        }
                    }
                }
                Err(error) => {
                    startup_results.push(
                        StoreStartupResult::optional("runtime_state_store")
                            .failed(error.to_string()),
                    );
                    tracing::warn!(
                        path = %runtime_state_db_path.display(),
                        "runtime state store context init failed, runtime host state will not persist: {error}"
                    );
                    None
                }
            },
        }
    };

    setup_pool.close().await;

    Ok(RegistryBundle {
        thread_db,
        plan_db,
        plan_cache,
        issue_workflow_store,
        project_workflow_store,
        workflow_runtime_store,
        project_registry,
        runtime_state_store,
        workspace_mgr,
        startup_results,
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
        let project_registry = bundle.project_registry.as_ref().unwrap_or_else(|| {
            panic!(
                "project registry should be ready: {:?}",
                bundle.startup_results
            )
        });
        let projects = project_registry.list().await.expect("list projects");
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

    #[tokio::test]
    async fn runtime_state_failure_is_recorded_as_optional() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (server, tasks) = make_test_server_and_tasks(dir.path()).await;
        let bundle = super::super::with_forced_startup_failures(
            &[(
                "runtime_state_store",
                "pool timed out while waiting for an open connection",
            )],
            build_registry(&server, dir.path(), dir.path(), &tasks),
        )
        .await
        .expect("build_registry should succeed");
        assert!(
            bundle.project_registry.is_some(),
            "critical stores should stay ready: {:?}",
            bundle.startup_results
        );
        assert!(
            bundle.runtime_state_store.is_none(),
            "optional runtime state store should be disabled"
        );
        let status = bundle
            .startup_results
            .iter()
            .find(|status| status.name == "runtime_state_store")
            .expect("runtime_state_store startup result");
        assert!(!status.is_critical());
        assert!(!status.ready);
    }

    #[tokio::test]
    async fn invalid_workflow_schema_namespace_disables_optional_workflow_stores() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("WORKFLOW.md"),
            "---\nstorage:\n  schema_namespace: invalid-name\n---\n",
        )
        .expect("write workflow config");
        let (server, tasks) = make_test_server_and_tasks(dir.path()).await;

        let bundle = build_registry(&server, dir.path(), dir.path(), &tasks)
            .await
            .expect("optional workflow-store context failures should not abort startup");

        assert!(
            bundle.project_registry.is_some(),
            "critical stores should stay ready: {:?}",
            bundle.startup_results
        );
        assert!(bundle.issue_workflow_store.is_none());
        assert!(bundle.project_workflow_store.is_none());
        for name in ["issue_workflow_store", "project_workflow_store"] {
            let status = bundle
                .startup_results
                .iter()
                .find(|status| status.name == name)
                .unwrap_or_else(|| panic!("{name} startup result should be recorded"));
            assert!(!status.is_critical());
            assert!(!status.ready);
        }
    }

    #[test]
    fn bootstrap_failure_records_workspace_manager_status() {
        let statuses = failed_registry_startup_results("database unavailable");
        let workspace_status = statuses
            .iter()
            .find(|status| status.name == "workspace_manager")
            .expect("workspace_manager status");

        assert!(!workspace_status.ready);
        assert!(!workspace_status.is_critical());
    }

    #[tokio::test]
    async fn project_registry_failure_is_recorded_as_critical() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (server, tasks) = make_test_server_and_tasks(dir.path()).await;
        let bundle = super::super::with_forced_startup_failures(
            &[("project_registry", "failed to open Postgres bootstrap pool")],
            build_registry(&server, dir.path(), dir.path(), &tasks),
        )
        .await
        .expect("build_registry should succeed");
        assert!(
            bundle.project_registry.is_none(),
            "critical project registry should be absent"
        );
        let status = bundle
            .startup_results
            .iter()
            .find(|status| status.name == "project_registry")
            .expect("project_registry startup result");
        assert!(status.is_critical());
        assert!(!status.ready);
    }
}
