use super::test_support::*;
use super::*;

#[tokio::test]
async fn reconcile_startup_removes_generation_drifted_workspace() {
    let source = tempfile::tempdir().expect("tempdir");
    init_git_repo(source.path());
    let branch = current_branch(source.path());

    let workspaces = tempfile::tempdir().expect("tempdir");
    let config = WorkspaceConfig {
        root: workspaces.path().to_path_buf(),
        ..Default::default()
    };
    let mgr_a = WorkspaceManager::new(config.clone()).expect("mgr a");
    let mgr_b = WorkspaceManager::new(config).expect("mgr b");
    let task_id = harness_core::types::TaskId("startup-reconcile-task".to_string());

    let lease = mgr_a
        .create_workspace(&task_id, source.path(), "origin", &branch, 1, None, None)
        .await
        .expect("create workspace");
    assert!(lease.workspace_path.exists());

    let task_summary = crate::task_runner::TaskSummary {
        id: task_id.clone(),
        status: crate::task_runner::TaskStatus::Pending,
        failure_kind: None,
        turn: 0,
        pr_url: None,
        error: None,
        source: None,
        parent_id: None,
        external_id: None,
        repo: None,
        description: None,
        created_at: None,
        phase: crate::task_runner::TaskPhase::Implement,
        depends_on: vec![],
        subtask_ids: vec![],
        project: Some(source.path().to_string_lossy().into_owned()),
        workspace_path: Some(lease.workspace_path.to_string_lossy().into_owned()),
        workspace_owner: Some(mgr_a.owner_session.clone()),
        run_generation: 2,
        task_kind: crate::task_runner::TaskKind::Prompt,
        workflow: None,
        scheduler: crate::task_runner::TaskSchedulerState::default(),
    };

    let summary = mgr_b
        .reconcile_startup(source.path(), &[task_summary])
        .await
        .expect("startup reconcile");
    assert_eq!(summary.removed, 1);
    assert!(
        !lease.workspace_path.exists(),
        "generation drift should be cleaned"
    );
}

#[tokio::test]
async fn reconcile_startup_preserves_live_persisted_lease_from_other_manager() -> anyhow::Result<()>
{
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }
    let source = tempfile::tempdir()?;
    init_git_repo(source.path());
    let branch = current_branch(source.path());

    let workspaces = tempfile::tempdir()?;
    let lease_db = tempfile::tempdir()?;
    let store = std::sync::Arc::new(
        WorkspaceLeaseStore::open(&lease_db.path().join("workspace-leases")).await?,
    );
    let config = WorkspaceConfig {
        root: workspaces.path().to_path_buf(),
        ..Default::default()
    };
    let mgr_a = WorkspaceManager::new_with_pool(
        config.clone(),
        WorkspacePoolConfig::default(),
        Some(store.clone()),
    )?;
    let mgr_b =
        WorkspaceManager::new_with_pool(config, WorkspacePoolConfig::default(), Some(store))?;
    let task_id = harness_core::types::TaskId("startup-live-lease-slot".to_string());

    let lease = mgr_a
        .create_workspace(
            &task_id,
            source.path(),
            "origin",
            &branch,
            1,
            Some("issue:42"),
            Some("owner/repo"),
        )
        .await?;
    let task_summary = crate::task_runner::TaskSummary {
        id: task_id.clone(),
        status: crate::task_runner::TaskStatus::Done,
        failure_kind: None,
        turn: 0,
        pr_url: None,
        error: None,
        source: None,
        parent_id: None,
        external_id: Some("issue:42".to_string()),
        repo: Some("owner/repo".to_string()),
        description: None,
        created_at: None,
        phase: crate::task_runner::TaskPhase::Implement,
        depends_on: vec![],
        subtask_ids: vec![],
        project: Some(source.path().to_string_lossy().into_owned()),
        workspace_path: Some(lease.workspace_path.to_string_lossy().into_owned()),
        workspace_owner: Some(mgr_a.owner_session.clone()),
        run_generation: 1,
        task_kind: crate::task_runner::TaskKind::Prompt,
        workflow: None,
        scheduler: crate::task_runner::TaskSchedulerState::default(),
    };

    let summary = mgr_b
        .reconcile_startup(source.path(), &[task_summary])
        .await?;

    assert_eq!(summary.removed, 0);
    assert_eq!(summary.preserved, 1);
    assert!(
        lease.workspace_path.exists(),
        "startup reconciliation must not remove a workspace with another live persisted lease"
    );

    mgr_a.remove_workspace(&task_id).await?;
    Ok(())
}

#[tokio::test]
async fn reconcile_startup_preserves_shared_issue_workspace_when_any_attempt_active() {
    let source = tempfile::tempdir().expect("tempdir");
    init_git_repo(source.path());
    let branch = current_branch(source.path());

    let workspaces = tempfile::tempdir().expect("tempdir");
    let config = WorkspaceConfig {
        root: workspaces.path().to_path_buf(),
        ..Default::default()
    };
    let mgr_a = WorkspaceManager::new(config.clone()).expect("mgr a");
    let mgr_b = WorkspaceManager::new(config).expect("mgr b");
    let active_task_id = harness_core::types::TaskId("active-issue-42-task".to_string());

    let lease = mgr_a
        .create_workspace(
            &active_task_id,
            source.path(),
            "origin",
            &branch,
            1,
            Some("issue:42"),
            Some("owner/repo"),
        )
        .await
        .expect("create issue workspace");
    std::fs::remove_file(owner_record_path(&lease.workspace_path).expect("owner path"))
        .expect("remove owner record");

    let mut active_task = crate::task_runner::TaskSummary {
        id: active_task_id,
        status: crate::task_runner::TaskStatus::Implementing,
        failure_kind: None,
        turn: 0,
        pr_url: None,
        error: None,
        source: None,
        parent_id: None,
        external_id: Some("issue:42".to_string()),
        repo: Some("owner/repo".to_string()),
        description: None,
        created_at: None,
        phase: crate::task_runner::TaskPhase::Implement,
        depends_on: vec![],
        subtask_ids: vec![],
        project: Some(source.path().to_string_lossy().into_owned()),
        workspace_path: Some(lease.workspace_path.to_string_lossy().into_owned()),
        workspace_owner: Some(mgr_a.owner_session.clone()),
        run_generation: 1,
        task_kind: crate::task_runner::TaskKind::Prompt,
        workflow: None,
        scheduler: crate::task_runner::TaskSchedulerState::default(),
    };
    let terminal_task_id = harness_core::types::TaskId("terminal-issue-42-task".to_string());
    let terminal_task = crate::task_runner::TaskSummary {
        id: terminal_task_id,
        status: crate::task_runner::TaskStatus::Failed,
        workspace_path: active_task.workspace_path.clone(),
        workspace_owner: Some(mgr_a.owner_session.clone()),
        project: active_task.project.clone(),
        external_id: active_task.external_id.clone(),
        repo: active_task.repo.clone(),
        ..active_task.clone()
    };
    active_task.status = crate::task_runner::TaskStatus::Implementing;

    let summary = mgr_b
        .reconcile_startup(source.path(), &[active_task, terminal_task])
        .await
        .expect("startup reconcile");

    assert_eq!(summary.preserved, 1);
    assert!(
        lease.workspace_path.exists(),
        "startup reconciliation must not delete a shared issue workspace while any attempt is active"
    );

    cleanup_workspace_path(source.path(), &lease.workspace_path)
        .await
        .expect("cleanup test workspace");
}

#[tokio::test]
async fn reconcile_startup_cleans_up_with_workspace_owning_repo() {
    let source_a = tempfile::tempdir().expect("tempdir");
    init_git_repo(source_a.path());
    let branch_a = current_branch(source_a.path());

    let source_b = tempfile::tempdir().expect("tempdir");
    init_git_repo(source_b.path());

    let workspaces = tempfile::tempdir().expect("tempdir");
    let config = WorkspaceConfig {
        root: workspaces.path().to_path_buf(),
        ..Default::default()
    };
    let mgr_a = WorkspaceManager::new(config.clone()).expect("mgr a");
    let mgr_b = WorkspaceManager::new(config).expect("mgr b");
    let task_id = harness_core::types::TaskId("startup-owning-repo-task".to_string());

    let lease = mgr_a
        .create_workspace(
            &task_id,
            source_a.path(),
            "origin",
            &branch_a,
            1,
            None,
            None,
        )
        .await
        .expect("create workspace");
    assert!(lease.workspace_path.exists());

    let task_summary = crate::task_runner::TaskSummary {
        id: task_id,
        status: crate::task_runner::TaskStatus::Pending,
        failure_kind: None,
        turn: 0,
        pr_url: None,
        error: None,
        source: None,
        parent_id: None,
        external_id: None,
        repo: None,
        description: None,
        created_at: None,
        phase: crate::task_runner::TaskPhase::Implement,
        depends_on: vec![],
        subtask_ids: vec![],
        project: Some(source_a.path().to_string_lossy().into_owned()),
        workspace_path: Some(lease.workspace_path.to_string_lossy().into_owned()),
        workspace_owner: Some(mgr_a.owner_session.clone()),
        run_generation: 2,
        task_kind: crate::task_runner::TaskKind::Prompt,
        workflow: None,
        scheduler: crate::task_runner::TaskSchedulerState::default(),
    };

    let summary = mgr_b
        .reconcile_startup(source_b.path(), &[task_summary])
        .await
        .expect("startup reconcile");
    assert_eq!(summary.removed, 1);
    assert!(
        !lease.workspace_path.exists(),
        "generation drift should be cleaned"
    );

    let listed = String::from_utf8(
        run_git(&[
            "-C",
            &source_a.path().to_string_lossy(),
            "worktree",
            "list",
            "--porcelain",
        ])
        .stdout,
    )
    .expect("utf8");
    assert!(
        !listed.lines().any(|line| {
            line.strip_prefix("worktree ")
                .is_some_and(|entry| entry == lease.workspace_path.to_string_lossy())
        }),
        "startup cleanup must prune the owning repo entry"
    );
}

#[tokio::test]
async fn cleanup_workspace_path_prunes_missing_registered_worktree() {
    let source = tempfile::tempdir().expect("tempdir");
    init_git_repo(source.path());
    let branch = current_branch(source.path());

    let workspaces = tempfile::tempdir().expect("tempdir");
    let config = WorkspaceConfig {
        root: workspaces.path().to_path_buf(),
        ..Default::default()
    };
    let mgr = WorkspaceManager::new(config).expect("new");
    let task_id = harness_core::types::TaskId("missing-registered-task".to_string());

    let lease = mgr
        .create_workspace(&task_id, source.path(), "origin", &branch, 1, None, None)
        .await
        .expect("create workspace");
    std::fs::remove_dir_all(&lease.workspace_path).expect("remove checkout dir");

    assert!(
        is_registered_worktree(source.path(), &lease.workspace_path).await,
        "git should still have a stale worktree registration"
    );

    cleanup_workspace_path(source.path(), &lease.workspace_path)
        .await
        .expect("cleanup missing registered worktree");

    assert!(
        !is_registered_worktree(source.path(), &lease.workspace_path).await,
        "cleanup should prune missing worktree registration"
    );
}

#[tokio::test]
async fn reconcile_startup_prunes_missing_registered_worktree_for_tracked_task() {
    let source = tempfile::tempdir().expect("tempdir");
    init_git_repo(source.path());
    let branch = current_branch(source.path());

    let workspaces = tempfile::tempdir().expect("tempdir");
    let config = WorkspaceConfig {
        root: workspaces.path().to_path_buf(),
        ..Default::default()
    };
    let mgr_a = WorkspaceManager::new(config.clone()).expect("mgr a");
    let mgr_b = WorkspaceManager::new(config).expect("mgr b");
    let task_id = harness_core::types::TaskId("startup-missing-registered-task".to_string());

    let lease = mgr_a
        .create_workspace(&task_id, source.path(), "origin", &branch, 1, None, None)
        .await
        .expect("create workspace");
    std::fs::remove_dir_all(&lease.workspace_path).expect("remove checkout dir");

    assert!(
        is_registered_worktree(source.path(), &lease.workspace_path).await,
        "git should still have a stale worktree registration"
    );

    let task_summary = crate::task_runner::TaskSummary {
        id: task_id.clone(),
        status: crate::task_runner::TaskStatus::Pending,
        failure_kind: None,
        turn: 0,
        pr_url: None,
        error: None,
        source: None,
        parent_id: None,
        external_id: None,
        repo: None,
        description: None,
        created_at: None,
        phase: crate::task_runner::TaskPhase::Implement,
        depends_on: vec![],
        subtask_ids: vec![],
        project: Some(source.path().to_string_lossy().into_owned()),
        workspace_path: Some(lease.workspace_path.to_string_lossy().into_owned()),
        workspace_owner: Some(mgr_a.owner_session.clone()),
        run_generation: 1,
        task_kind: crate::task_runner::TaskKind::Prompt,
        workflow: None,
        scheduler: crate::task_runner::TaskSchedulerState::default(),
    };

    let summary = mgr_b
        .reconcile_startup(source.path(), &[task_summary])
        .await
        .expect("startup reconcile");
    assert_eq!(summary.removed, 1);
    assert!(
        !is_registered_worktree(source.path(), &lease.workspace_path).await,
        "startup reconcile should prune missing worktree registration"
    );

    let recreated = mgr_b
        .create_workspace(&task_id, source.path(), "origin", &branch, 2, None, None)
        .await
        .expect("recreate workspace after startup reconcile");
    assert!(
        recreated.workspace_path.exists(),
        "startup reconcile should unblock the next worktree add"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn reconcile_startup_continues_after_workspace_cleanup_failure() {
    use std::os::unix::fs::PermissionsExt;

    let source = tempfile::tempdir().expect("tempdir");
    init_git_repo(source.path());

    let workspaces = tempfile::tempdir().expect("tempdir");
    let config = WorkspaceConfig {
        root: workspaces.path().to_path_buf(),
        ..Default::default()
    };
    let mgr = WorkspaceManager::new(config).expect("new");

    let bad_path = workspaces.path().join("bad-stale-workspace");
    std::fs::create_dir_all(&bad_path).expect("create bad workspace");
    std::fs::write(bad_path.join("locked"), "locked").expect("write locked child");
    std::fs::set_permissions(&bad_path, std::fs::Permissions::from_mode(0o500))
        .expect("lock bad workspace");

    let good_path = workspaces.path().join("good-stale-workspace");
    std::fs::create_dir_all(&good_path).expect("create good workspace");

    let summary = mgr
        .reconcile_startup(source.path(), &[])
        .await
        .expect("startup reconcile should continue after per-entry cleanup failure");

    std::fs::set_permissions(&bad_path, std::fs::Permissions::from_mode(0o700))
        .expect("unlock bad workspace");

    assert_eq!(
        summary.removed, 1,
        "successful entries should still be counted after another entry fails"
    );
    assert!(
        !good_path.exists(),
        "good stale workspace should still be cleaned"
    );
    assert!(
        bad_path.exists(),
        "failed stale workspace should remain for later retry or operator cleanup"
    );
}

struct CwdGuard {
    original: PathBuf,
    _guard: tokio::sync::MutexGuard<'static, ()>,
}

impl CwdGuard {
    async fn switch_to(path: &Path) -> anyhow::Result<Self> {
        let guard = async_cwd_lock().lock().await;
        let original = std::env::current_dir()?;
        std::env::set_current_dir(path)?;
        Ok(Self {
            original,
            _guard: guard,
        })
    }
}

impl Drop for CwdGuard {
    fn drop(&mut self) {
        let _ = std::env::set_current_dir(&self.original);
    }
}

#[tokio::test]
async fn is_registered_worktree_matches_relative_workspace_paths() {
    let _guard = async_env_lock().lock().await;

    let sandbox = tempfile::tempdir().expect("tempdir");
    let _cwd_guard = CwdGuard::switch_to(sandbox.path())
        .await
        .expect("switch cwd");

    let source = sandbox.path().join("source");
    std::fs::create_dir_all(&source).expect("create source");
    init_git_repo(&source);
    let branch = current_branch(&source);

    let config = WorkspaceConfig {
        root: PathBuf::from("workspaces"),
        ..Default::default()
    };
    let mgr = WorkspaceManager::new(config).expect("new");
    let task_id = harness_core::types::TaskId("test-task-relative-path".to_string());

    let lease = mgr
        .create_workspace(&task_id, &source, "origin", &branch, 1, None, None)
        .await
        .expect("create");
    let relative_workspace_path = lease
        .workspace_path
        .strip_prefix(std::fs::canonicalize(sandbox.path()).expect("canonical sandbox"))
        .expect("workspace should be under sandbox")
        .to_path_buf();

    assert!(
        is_registered_worktree(&source, &relative_workspace_path).await,
        "registered worktree lookup should canonicalize relative paths"
    );

    mgr.remove_workspace(&task_id).await.expect("remove");
}

#[tokio::test]
async fn cleanup_terminal_removes_all_workspaces() {
    let source = tempfile::tempdir().expect("tempdir");
    init_git_repo(source.path());
    let branch = current_branch(source.path());

    let workspaces = tempfile::tempdir().expect("tempdir");
    let config = WorkspaceConfig {
        root: workspaces.path().to_path_buf(),
        ..Default::default()
    };
    let mgr = Arc::new(WorkspaceManager::new(config).expect("new"));

    let ids: Vec<TaskId> = (1..=3)
        .map(|i| harness_core::types::TaskId(format!("cleanup-task-{i:03}")))
        .collect();

    for id in &ids {
        mgr.create_workspace(id, source.path(), "origin", &branch, 1, None, None)
            .await
            .expect("create");
    }

    mgr.cleanup_terminal(&ids).await.expect("cleanup_terminal");

    for id in &ids {
        assert!(
            mgr.get_workspace(id).is_none(),
            "workspace for {id:?} should have been cleaned up"
        );
    }
}

// ── GC trigger demotion tests (issue #969) ────────────────────────────

fn make_task_summary(
    id: &str,
    status: crate::task_runner::TaskStatus,
    external_id: Option<&str>,
) -> crate::task_runner::TaskSummary {
    crate::task_runner::TaskSummary {
        id: harness_core::types::TaskId(id.to_string()),
        status,
        failure_kind: None,
        turn: 0,
        pr_url: None,
        error: None,
        source: None,
        parent_id: None,
        external_id: external_id.map(|s| s.to_string()),
        repo: None,
        description: None,
        created_at: None,
        phase: crate::task_runner::TaskPhase::Implement,
        depends_on: vec![],
        subtask_ids: vec![],
        project: None,
        workspace_path: None,
        workspace_owner: None,
        run_generation: 1,
        task_kind: crate::task_runner::TaskKind::Prompt,
        workflow: None,
        scheduler: crate::task_runner::TaskSchedulerState::default(),
    }
}

/// reconcile_startup counts issue-keyed terminal dirs as `migrated`, not `removed`.
/// Uses separate managers so the workspace dirs are not tracked as active by the
/// reconciling manager (matching the real server startup scenario).
#[tokio::test]
async fn reconcile_startup_migration_counts_new_key_terminal_as_migrated() {
    let source = tempfile::tempdir().expect("tempdir");
    init_git_repo(source.path());
    let branch = current_branch(source.path());

    let workspaces = tempfile::tempdir().expect("tempdir");
    let config = WorkspaceConfig {
        root: workspaces.path().to_path_buf(),
        ..Default::default()
    };

    // mgr_a creates the UUID workspace (simulates the previous server session).
    let mgr_a = WorkspaceManager::new(config.clone()).expect("mgr_a");
    let uuid_id = harness_core::types::TaskId("uuid-task-migration-123".to_string());
    let uuid_lease = mgr_a
        .create_workspace(&uuid_id, source.path(), "origin", &branch, 1, None, None)
        .await
        .expect("create uuid workspace");

    // Simulate a new-key workspace: dir with owner record where task_id = "issue:42".
    let issue_dir = workspaces.path().join("issue_42");
    std::fs::create_dir_all(issue_dir.join(".git")).expect("create issue dir");
    std::fs::write(
        issue_dir.join(".git").join(OWNER_RECORD_FILE),
        serde_json::to_vec(&WorkspaceOwnerRecord {
            task_id: "issue:42".to_string(),
            run_generation: 1,
            owner_session: "test-session".to_string(),
            workspace_key: None,
        })
        .expect("serialize"),
    )
    .expect("write owner record");

    // mgr_b is the fresh server-startup manager — has no active workspaces.
    let mgr_b = WorkspaceManager::new(config).expect("mgr_b");
    let uuid_task = make_task_summary(
        "uuid-task-migration-123",
        crate::task_runner::TaskStatus::Done,
        None,
    );

    let summary = mgr_b
        .reconcile_startup(source.path(), &[uuid_task])
        .await
        .expect("reconcile startup");

    assert_eq!(summary.removed, 1, "UUID terminal dir counted as removed");
    assert_eq!(
        summary.migrated, 1,
        "issue-keyed terminal dir counted as migrated"
    );
    assert!(
        !uuid_lease.workspace_path.exists(),
        "uuid workspace cleaned up"
    );
    assert!(!issue_dir.exists(), "issue-keyed workspace cleaned up");
}

/// reconcile_startup orphan UUID dir (no task) → removed, migrated stays 0.
#[tokio::test]
async fn reconcile_startup_uuid_orphan_removed() {
    let source = tempfile::tempdir().expect("tempdir");
    init_git_repo(source.path());
    let branch = current_branch(source.path());

    let workspaces = tempfile::tempdir().expect("tempdir");
    let config = WorkspaceConfig {
        root: workspaces.path().to_path_buf(),
        ..Default::default()
    };
    // mgr_a creates the workspace; mgr_b is the fresh startup manager.
    let mgr_a = WorkspaceManager::new(config.clone()).expect("mgr_a");
    let task_id = harness_core::types::TaskId("orphan-uuid-task".to_string());

    let lease = mgr_a
        .create_workspace(&task_id, source.path(), "origin", &branch, 1, None, None)
        .await
        .expect("create workspace");

    let mgr_b = WorkspaceManager::new(config).expect("mgr_b");
    let summary = mgr_b
        .reconcile_startup(source.path(), &[])
        .await
        .expect("reconcile startup");

    assert_eq!(summary.removed, 1);
    assert_eq!(summary.migrated, 0);
    assert!(
        !lease.workspace_path.exists(),
        "orphan uuid workspace removed"
    );
}
