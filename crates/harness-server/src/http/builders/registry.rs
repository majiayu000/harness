use dashmap::DashMap;
use std::path::Path;
use std::sync::Arc;

use crate::server::HarnessServer;

/// Outputs of the registry initialization phase.
pub(crate) struct RegistryBundle {
    pub thread_db: crate::thread_db::ThreadDb,
    pub plan_db: crate::plan_db::PlanDb,
    pub plan_cache: Arc<DashMap<String, harness_exec::plan::ExecPlan>>,
    pub issue_workflow_store: Option<Arc<harness_workflow::issue_lifecycle::IssueWorkflowStore>>,
    pub project_workflow_store:
        Option<Arc<harness_workflow::project_lifecycle::ProjectWorkflowStore>>,
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
    let configured_database_url = server.config.server.database_url.as_deref();
    let workflow_config =
        harness_core::config::workflow::load_workflow_config(project_root).unwrap_or_default();
    let workflow_ns = workflow_config.storage.schema_namespace;
    // ── Thread DB ─────────────────────────────────────────────────────────────
    let thread_db_path = harness_core::config::dirs::default_db_path(data_dir, "threads");
    let thread_db = crate::thread_db::ThreadDb::open_with_database_url(
        &thread_db_path,
        configured_database_url,
    )
    .await?;

    // Load persisted threads into the in-memory ThreadManager cache.
    for thread in thread_db.list().await? {
        server
            .thread_manager
            .threads_cache()
            .insert(thread.id.as_str().to_string(), thread);
    }

    // ── Plan DB + cache ───────────────────────────────────────────────────────
    let plan_db = crate::plan_db::PlanDb::open_with_database_url(
        &harness_core::config::dirs::default_db_path(data_dir, "plans"),
        configured_database_url,
    )
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

    // ── Issue workflow store ──────────────────────────────────────────────────
    let issue_workflows_db_path =
        harness_core::config::dirs::default_db_path(data_dir, "issue_workflows");
    let issue_workflow_store = {
        let schema = format!("{workflow_ns}_issue");
        match harness_workflow::issue_lifecycle::IssueWorkflowStore::open_with_database_url_and_schema(
            configured_database_url,
            &schema,
        )
        .await
        {
            Ok(store) => {
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
                tracing::warn!(
                    schema = %schema,
                    "issue workflow store init failed, issue lifecycle state will not persist: {e}"
                );
                None
            }
        }
    };

    // ── Project workflow store ────────────────────────────────────────────────
    let project_workflows_db_path =
        harness_core::config::dirs::default_db_path(data_dir, "project_workflows");
    let project_workflow_store = {
        let schema = format!("{workflow_ns}_project");
        match harness_workflow::project_lifecycle::ProjectWorkflowStore::open_with_database_url_and_schema(
            configured_database_url,
            &schema,
        )
        .await
        {
            Ok(store) => {
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
                tracing::warn!(
                    schema = %schema,
                    "project workflow store init failed, project lifecycle state will not persist: {e}"
                );
                None
            }
        }
    };

    // ── Project registry ──────────────────────────────────────────────────────
    let project_registry = crate::project_registry::ProjectRegistry::open_with_database_url(
        &harness_core::config::dirs::default_db_path(data_dir, "projects"),
        configured_database_url,
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
        match crate::runtime_state_store::RuntimeStateStore::open_with_database_url(
            &runtime_state_db_path,
            configured_database_url,
        )
        .await
        {
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
        issue_workflow_store,
        project_workflow_store,
        project_registry,
        runtime_state_store,
        workspace_mgr,
    })
}

/// Walk all `issue_workflows` rows. For each row whose `project_id` contains
/// `/workspaces/` (a corrupt worktree path), replace it with `canonical_root`.
/// Returns `(rewritten, failed, skipped)` counts.
async fn repair_corrupt_project_ids(
    store: &harness_workflow::issue_lifecycle::IssueWorkflowStore,
    canonical_root: &std::path::Path,
) -> (u64, u64, u64) {
    let corrupt_rows = match store.list_with_worktree_project_ids().await {
        Ok(rows) => rows,
        Err(e) => {
            tracing::error!("startup repair: failed to list corrupt issue workflows: {e}");
            return (0, 0, 0);
        }
    };
    let total_count = store.row_count().await.unwrap_or(0);

    let new_project_id = canonical_root.to_string_lossy().into_owned();
    let (mut rewritten, mut failed) = (0u64, 0u64);
    let skipped = (total_count as u64).saturating_sub(corrupt_rows.len() as u64);

    for workflow in corrupt_rows {
        tracing::info!(
            row_id = %workflow.id,
            old_project_id = %workflow.project_id,
            new_project_id = %new_project_id,
            "startup repair: rewriting corrupt workflow project_id"
        );
        match store.repair_project_id(&workflow.id, &new_project_id).await {
            Ok(()) => rewritten += 1,
            Err(e) => {
                tracing::error!(
                    row_id = %workflow.id,
                    "startup repair: failed to rewrite project_id: {e}; marking workflow failed"
                );
                if let Err(e2) = store
                    .mark_workflow_failed_with_reason(
                        &workflow.id,
                        "failed to repair project_id during startup",
                    )
                    .await
                {
                    tracing::error!(
                        row_id = %workflow.id,
                        "startup repair: also failed to mark workflow as failed: {e2}"
                    );
                }
                failed += 1;
            }
        }
    }

    (rewritten, failed, skipped)
}

async fn migrate_issue_workflows_if_needed(
    configured_database_url: Option<&str>,
    legacy_path: &Path,
    target_schema: &str,
    target_store: &harness_workflow::issue_lifecycle::IssueWorkflowStore,
) -> anyhow::Result<()> {
    let legacy_schema = harness_workflow::issue_lifecycle::legacy_schema_for_path(legacy_path)?;
    if legacy_schema == target_schema {
        return Ok(());
    }

    let legacy_store =
        harness_workflow::issue_lifecycle::IssueWorkflowStore::open_with_database_url(
            legacy_path,
            configured_database_url,
        )
        .await?;
    let legacy_rows = legacy_store.list().await?;
    if legacy_rows.is_empty() {
        return Ok(());
    }

    let mut copied = 0usize;
    let mut skipped_existing = 0usize;
    for workflow in &legacy_rows {
        if target_store.contains_id(&workflow.id).await? {
            skipped_existing += 1;
            continue;
        }
        target_store.upsert(workflow).await?;
        copied += 1;
    }
    tracing::info!(
        legacy_count = legacy_rows.len(),
        copied,
        skipped_existing,
        legacy_schema = %legacy_schema,
        target_schema = %target_schema,
        "workflow migration: copied legacy issue workflow rows into namespaced schema"
    );
    Ok(())
}

async fn migrate_project_workflows_if_needed(
    configured_database_url: Option<&str>,
    legacy_path: &Path,
    target_schema: &str,
    target_store: &harness_workflow::project_lifecycle::ProjectWorkflowStore,
) -> anyhow::Result<()> {
    let legacy_schema = harness_workflow::project_lifecycle::legacy_schema_for_path(legacy_path)?;
    if legacy_schema == target_schema {
        return Ok(());
    }

    let legacy_store =
        harness_workflow::project_lifecycle::ProjectWorkflowStore::open_with_database_url(
            legacy_path,
            configured_database_url,
        )
        .await?;
    let legacy_rows = legacy_store.list().await?;
    if legacy_rows.is_empty() {
        return Ok(());
    }

    let mut copied = 0usize;
    let mut skipped_existing = 0usize;
    for workflow in &legacy_rows {
        if target_store.contains_id(&workflow.id).await? {
            skipped_existing += 1;
            continue;
        }
        target_store.upsert(workflow).await?;
        copied += 1;
    }
    tracing::info!(
        legacy_count = legacy_rows.len(),
        copied,
        skipped_existing,
        legacy_schema = %legacy_schema,
        target_schema = %target_schema,
        "workflow migration: copied legacy project workflow rows into namespaced schema"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{server::HarnessServer, thread_manager::ThreadManager};
    use harness_agents::registry::AgentRegistry;
    use harness_core::{
        config::HarnessConfig,
        db::{pg_schema_for_path, resolve_database_url},
    };

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

    async fn open_test_issue_store(
    ) -> anyhow::Result<Option<harness_workflow::issue_lifecycle::IssueWorkflowStore>> {
        if resolve_database_url(None).is_err() {
            return Ok(None);
        }
        let dir = tempfile::tempdir()?;
        match harness_workflow::issue_lifecycle::IssueWorkflowStore::open(
            &dir.path().join("issue_workflows.db"),
        )
        .await
        {
            Ok(store) => Ok(Some(store)),
            Err(e) => {
                tracing::warn!("issue workflow store test skipped: {e}");
                Ok(None)
            }
        }
    }

    #[tokio::test]
    async fn migration_rewrites_worktree_paths() -> anyhow::Result<()> {
        let Some(store) = open_test_issue_store().await? else {
            return Ok(());
        };
        let corrupt = "/data/workspaces/abc-uuid-reg-test";
        let canonical = std::path::Path::new("/real/canonical/reg-root");

        store
            .record_issue_scheduled(corrupt, Some("owner/repo"), 8001, "task-reg1", &[], false)
            .await?;
        store
            .record_issue_scheduled(corrupt, Some("owner/repo"), 8002, "task-reg2", &[], false)
            .await?;

        let (rewritten, failed, skipped) = repair_corrupt_project_ids(&store, canonical).await;
        assert_eq!(rewritten, 2, "both worktree rows should be rewritten");
        assert_eq!(failed, 0);
        assert_eq!(skipped, 0);

        let canonical_str = canonical.to_string_lossy();
        let r1 = store
            .get_by_issue(&canonical_str, Some("owner/repo"), 8001)
            .await?;
        assert!(r1.is_some(), "row 8001 should have canonical project_id");
        let r2 = store
            .get_by_issue(&canonical_str, Some("owner/repo"), 8002)
            .await?;
        assert!(r2.is_some(), "row 8002 should have canonical project_id");
        Ok(())
    }

    #[tokio::test]
    async fn migration_skips_canonical_rows() -> anyhow::Result<()> {
        let Some(store) = open_test_issue_store().await? else {
            return Ok(());
        };
        let canonical_id = "/tmp/already-canonical-skip-test";
        let canonical_root = std::path::Path::new("/any/root");

        store
            .record_issue_scheduled(
                canonical_id,
                Some("owner/repo"),
                8003,
                "task-skip1",
                &[],
                false,
            )
            .await?;

        let (rewritten, failed, skipped) = repair_corrupt_project_ids(&store, canonical_root).await;
        assert_eq!(skipped, 1, "canonical row should be skipped");
        assert_eq!(rewritten, 0);
        assert_eq!(failed, 0);

        // Row untouched: project_id unchanged
        let row = store
            .get_by_issue(canonical_id, Some("owner/repo"), 8003)
            .await?
            .expect("row should still exist");
        assert_eq!(row.project_id, canonical_id);
        Ok(())
    }

    #[tokio::test]
    async fn migration_counts_all_categories() -> anyhow::Result<()> {
        let Some(store) = open_test_issue_store().await? else {
            return Ok(());
        };
        let corrupt = "/data/workspaces/mix-uuid-reg-test";
        let canonical_id = "/tmp/canonical-mix-test";
        let canonical_root = std::path::Path::new("/real/mix-root");

        store
            .record_issue_scheduled(corrupt, Some("owner/repo"), 8010, "task-mix1", &[], false)
            .await?;
        store
            .record_issue_scheduled(
                canonical_id,
                Some("owner/repo"),
                8011,
                "task-mix2",
                &[],
                false,
            )
            .await?;

        let (rewritten, failed, skipped) = repair_corrupt_project_ids(&store, canonical_root).await;
        assert_eq!(rewritten, 1);
        assert_eq!(failed, 0);
        assert_eq!(skipped, 1);
        Ok(())
    }

    #[tokio::test]
    async fn issue_workflow_migration_backfills_partial_target() -> anyhow::Result<()> {
        let configured_database_url = match resolve_database_url(None) {
            Ok(url) => Some(url),
            Err(_) => return Ok(()),
        };
        let dir = tempfile::tempdir()?;
        let legacy_path = dir.path().join("issue_workflows.db");
        let target_schema = pg_schema_for_path(&dir.path().join("issue_workflows_target.db"))?;

        let legacy_store =
            harness_workflow::issue_lifecycle::IssueWorkflowStore::open_with_database_url(
                &legacy_path,
                configured_database_url.as_deref(),
            )
            .await?;
        let target_store =
            harness_workflow::issue_lifecycle::IssueWorkflowStore::open_with_database_url_and_schema(
                configured_database_url.as_deref(),
                &target_schema,
            )
            .await?;

        legacy_store
            .record_issue_scheduled(
                "/tmp/project",
                Some("owner/repo"),
                9101,
                "task-9101",
                &[],
                false,
            )
            .await?;
        legacy_store
            .record_issue_scheduled(
                "/tmp/project",
                Some("owner/repo"),
                9102,
                "task-9102",
                &[],
                false,
            )
            .await?;
        target_store
            .record_issue_scheduled(
                "/tmp/project",
                Some("owner/repo"),
                9101,
                "target-task-9101",
                &[],
                false,
            )
            .await?;
        target_store
            .record_pr_detected(
                "/tmp/project",
                Some("owner/repo"),
                9101,
                "target-pr-task-9101",
                99101,
                "https://github.com/owner/repo/pull/99101",
            )
            .await?;

        migrate_issue_workflows_if_needed(
            configured_database_url.as_deref(),
            &legacy_path,
            &target_schema,
            &target_store,
        )
        .await?;

        let existing = target_store
            .get_by_issue("/tmp/project", Some("owner/repo"), 9101)
            .await?
            .expect("existing target row should remain present");
        assert_eq!(
            existing.state,
            harness_workflow::issue_lifecycle::IssueLifecycleState::PrOpen,
            "existing target row state should not be overwritten"
        );
        assert_eq!(existing.pr_number, Some(99101));
        assert_eq!(
            existing.active_task_id.as_deref(),
            Some("target-pr-task-9101")
        );
        assert!(
            target_store
                .get_by_issue("/tmp/project", Some("owner/repo"), 9102)
                .await?
                .is_some(),
            "missing legacy row should be backfilled even when target is non-empty"
        );
        Ok(())
    }

    #[tokio::test]
    async fn project_workflow_migration_backfills_partial_target() -> anyhow::Result<()> {
        let configured_database_url = match resolve_database_url(None) {
            Ok(url) => Some(url),
            Err(_) => return Ok(()),
        };
        let dir = tempfile::tempdir()?;
        let legacy_path = dir.path().join("project_workflows.db");
        let target_schema = pg_schema_for_path(&dir.path().join("project_workflows_target.db"))?;

        let legacy_store =
            harness_workflow::project_lifecycle::ProjectWorkflowStore::open_with_database_url(
                &legacy_path,
                configured_database_url.as_deref(),
            )
            .await?;
        let target_store =
            harness_workflow::project_lifecycle::ProjectWorkflowStore::open_with_database_url_and_schema(
                configured_database_url.as_deref(),
                &target_schema,
            )
            .await?;

        legacy_store
            .record_poll_started("/tmp/project", Some("owner/repo"))
            .await?;
        legacy_store
            .record_poll_started("/tmp/project", Some("owner/repo-two"))
            .await?;
        target_store
            .record_poll_started("/tmp/project", Some("owner/repo"))
            .await?;
        target_store
            .record_degraded("/tmp/project", Some("owner/repo"), "target is canonical")
            .await?;

        migrate_project_workflows_if_needed(
            configured_database_url.as_deref(),
            &legacy_path,
            &target_schema,
            &target_store,
        )
        .await?;

        let existing = target_store
            .get_by_project("/tmp/project", Some("owner/repo"))
            .await?
            .expect("existing target row should remain present");
        assert_eq!(
            existing.state,
            harness_workflow::project_lifecycle::ProjectWorkflowState::Degraded,
            "existing target row state should not be overwritten"
        );
        assert_eq!(
            existing.degraded_reason.as_deref(),
            Some("target is canonical")
        );
        assert!(
            target_store
                .get_by_project("/tmp/project", Some("owner/repo-two"))
                .await?
                .is_some(),
            "missing legacy row should be backfilled even when target is non-empty"
        );
        Ok(())
    }
}
