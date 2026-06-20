use dashmap::DashMap;
use std::path::Path;
use std::sync::Arc;

use super::registry_failures::failed_registry_bundle;
use super::registry_migration::{
    migrate_issue_workflows_if_needed, migrate_project_workflows_if_needed,
    repair_corrupt_project_ids,
};
use crate::http::state::StoreStartupResult;
use crate::plan_db::PlanDb;
use crate::server::HarnessServer;

/// Outputs of the registry initialization phase.
pub(crate) struct RegistryBundle {
    pub thread_db: Option<crate::thread_db::ThreadDb>,
    pub plan_db: Option<PlanDb>,
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

    let thread_db_path = harness_core::config::dirs::default_db_path(data_dir, "threads");
    let thread_context = crate::thread_db::ThreadDb::shared_schema_context(Some(&database_url))?;
    super::ensure_startup_context_not_path_derived("thread_db", &thread_context)?;
    let thread_db = match super::forced_startup_error("thread_db") {
        Some(error) => {
            startup_results.push(StoreStartupResult::critical("thread_db").failed(error));
            None
        }
        None => match async {
            let thread_db = crate::thread_db::ThreadDb::open_shared_with_data_dir(
                &thread_context,
                &setup_pool,
                data_dir,
            )
            .await?;
            crate::thread_db::migrate_legacy_thread_db_if_needed(
                &thread_db_path,
                Some(&database_url),
                &thread_db,
            )
            .await?;
            Ok::<_, anyhow::Error>(thread_db)
        }
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

    let plans_db_path = harness_core::config::dirs::default_db_path(data_dir, "plans");
    let plan_db = match super::forced_startup_error("plan_db") {
        Some(error) => {
            startup_results.push(StoreStartupResult::critical("plan_db").failed(error));
            None
        }
        None => {
            match async {
                let plan_context = PlanDb::shared_schema_context(Some(&database_url))?;
                super::ensure_startup_context_not_path_derived("plan_db", &plan_context)?;
                let plan_db =
                    PlanDb::open_shared_with_data_dir(&plan_context, &setup_pool, data_dir).await?;
                crate::plan_db::migrate_legacy_plan_db_if_needed(
                    &plans_db_path,
                    Some(&database_url),
                    &plan_db,
                )
                .await?;
                Ok::<_, anyhow::Error>(plan_db)
            }
            .await
            {
                Ok(plan_db) => {
                    startup_results.push(StoreStartupResult::critical("plan_db"));
                    Some(plan_db)
                }
                Err(error) => {
                    startup_results
                        .push(StoreStartupResult::critical("plan_db").failed(error.to_string()));
                    None
                }
            }
        }
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
            None => match async {
                let context =
                    harness_core::db::PgStoreContext::new(database_url.clone(), schema.clone())?;
                super::ensure_startup_context_not_path_derived("issue_workflow_store", &context)?;
                let store =
                    harness_workflow::issue_lifecycle::IssueWorkflowStore::open_with_context(
                        &context,
                        &setup_pool,
                    )
                    .await?;
                if let Err(e) = migrate_issue_workflows_if_needed(
                    configured_database_url,
                    &setup_pool,
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
                Ok::<_, anyhow::Error>(store)
            }
            .await
            {
                Ok(store) => {
                    startup_results.push(StoreStartupResult::optional("issue_workflow_store"));
                    Some(Arc::new(store))
                }
                Err(error) => {
                    startup_results.push(
                        StoreStartupResult::optional("issue_workflow_store")
                            .failed(error.to_string()),
                    );
                    tracing::warn!(
                        schema = %schema,
                        "issue workflow store init failed, issue lifecycle state will not persist: {error}"
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
            None => match async {
                let context =
                    harness_core::db::PgStoreContext::new(database_url.clone(), schema.clone())?;
                super::ensure_startup_context_not_path_derived("project_workflow_store", &context)?;
                let store =
                    harness_workflow::project_lifecycle::ProjectWorkflowStore::open_with_context(
                        &context,
                        &setup_pool,
                    )
                    .await?;
                if let Err(e) = migrate_project_workflows_if_needed(
                    configured_database_url,
                    &setup_pool,
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
                Ok::<_, anyhow::Error>(store)
            }
            .await
            {
                Ok(store) => {
                    startup_results.push(StoreStartupResult::optional("project_workflow_store"));
                    Some(Arc::new(store))
                }
                Err(error) => {
                    startup_results.push(
                        StoreStartupResult::optional("project_workflow_store")
                            .failed(error.to_string()),
                    );
                    tracing::warn!(
                        schema = %schema,
                        "project workflow store init failed, project lifecycle state will not persist: {error}"
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
            None => match async {
                let context =
                    harness_core::db::PgStoreContext::new(database_url.clone(), schema.clone())?;
                super::ensure_startup_context_not_path_derived("workflow_runtime_store", &context)?;
                let store = harness_workflow::runtime::WorkflowRuntimeStore::open_with_context(
                    &context,
                    &setup_pool,
                )
                .await?;
                Ok::<_, anyhow::Error>(store)
            }
            .await
            {
                Ok(store) => {
                    startup_results.push(StoreStartupResult::optional("workflow_runtime_store"));
                    Some(Arc::new(store))
                }
                Err(error) => {
                    startup_results.push(
                        StoreStartupResult::optional("workflow_runtime_store")
                            .failed(error.to_string()),
                    );
                    tracing::warn!(
                        schema = %schema,
                        "workflow runtime store init failed, generic workflow decisions will not persist: {error}"
                    );
                    None
                }
            },
        }
    };

    // ── Project registry ──────────────────────────────────────────────────────
    let projects_db_path = harness_core::config::dirs::default_db_path(data_dir, "projects");
    let project_context =
        crate::project_registry::ProjectRegistry::shared_schema_context(Some(&database_url))?;
    super::ensure_startup_context_not_path_derived("project_registry", &project_context)?;
    let project_registry = match super::forced_startup_error("project_registry") {
        Some(error) => {
            startup_results.push(StoreStartupResult::critical("project_registry").failed(error));
            None
        }
        None => {
            match async {
                let project_registry =
                    crate::project_registry::ProjectRegistry::open_shared_with_data_dir(
                        &project_context,
                        &setup_pool,
                        data_dir,
                    )
                    .await?;
                crate::project_registry::migrate_legacy_project_registry_if_needed(
                    &projects_db_path,
                    Some(&database_url),
                    &project_registry,
                )
                .await?;
                Ok::<_, anyhow::Error>(project_registry)
            }
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
            }
        }
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
    let task_context = crate::task_db::TaskDb::shared_schema_context(Some(&database_url))?;
    super::ensure_startup_context_not_path_derived("workspace_lease_store", &task_context)?;
    let workspace_lease_store = match super::forced_startup_error("workspace_lease_store") {
        Some(error) => {
            startup_results
                .push(StoreStartupResult::optional("workspace_lease_store").failed(error));
            None
        }
        None => match crate::workspace_lease_store::WorkspaceLeaseStore::open_shared_with_data_dir(
            &task_context,
            &setup_pool,
            data_dir,
        )
        .await
        {
            Ok(store) => {
                startup_results.push(StoreStartupResult::optional("workspace_lease_store"));
                Some(Arc::new(store))
            }
            Err(error) => {
                startup_results.push(
                    StoreStartupResult::optional("workspace_lease_store").failed(error.to_string()),
                );
                tracing::warn!(
                    schema = %crate::task_db::TASK_DB_SCHEMA,
                    "workspace lease store init failed; workspace leases will not persist: {error}"
                );
                None
            }
        },
    };

    let workspace_pool_config = super::workspace_pool_config::build_workspace_pool_config(
        server,
        project_registry.as_ref(),
    )
    .await;
    let workspace_mgr = match crate::workspace::WorkspaceManager::new_with_pool(
        server.config.workspace.clone(),
        workspace_pool_config,
        workspace_lease_store,
    ) {
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
                            released_leases = summary.released_leases,
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
            None => match async {
                let context = crate::runtime_state_store::RuntimeStateStore::shared_schema_context(
                    Some(&database_url),
                )?;
                super::ensure_startup_context_not_path_derived("runtime_state_store", &context)?;
                let store =
                    crate::runtime_state_store::RuntimeStateStore::open_shared_with_data_dir(
                        &context,
                        &setup_pool,
                        data_dir,
                    )
                    .await?;
                crate::runtime_state_store::migrate_legacy_runtime_state_store_if_needed(
                    &runtime_state_db_path,
                    Some(&database_url),
                    &store,
                )
                .await?;
                Ok::<_, anyhow::Error>(store)
            }
            .await
            {
                Ok(store) => {
                    startup_results.push(StoreStartupResult::optional("runtime_state_store"));
                    Some(Arc::new(store))
                }
                Err(error) => {
                    startup_results.push(
                        StoreStartupResult::optional("runtime_state_store")
                            .failed(error.to_string()),
                    );
                    tracing::warn!(
                        path = %runtime_state_db_path.display(),
                        schema = crate::runtime_state_store::RUNTIME_STATE_STORE_SCHEMA,
                        "runtime state store init failed, runtime host state will not persist: {error}"
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
#[path = "registry_tests.rs"]
mod tests;
