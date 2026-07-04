use super::test_support::*;
use super::*;

#[tokio::test]
async fn reconcile_disk_skips_uuid_keyed_workspace() {
    let source = tempfile::tempdir().expect("tempdir");
    init_git_repo(source.path());
    let branch = current_branch(source.path());

    let workspaces = tempfile::tempdir().expect("tempdir");
    let config = WorkspaceConfig {
        root: workspaces.path().to_path_buf(),
        ..Default::default()
    };
    let mgr = WorkspaceManager::new(config).expect("mgr");
    let task_id = harness_core::types::TaskId("some-uuid-task".to_string());

    let lease = mgr
        .create_workspace(&task_id, source.path(), "origin", &branch, 1, None, None)
        .await
        .expect("create workspace");
    mgr.release_workspace(&task_id).await;

    let summary = mgr
        .reconcile_disk_workspaces(source.path(), "gh", 20, None)
        .await;

    assert_eq!(summary.skipped_uuid, 1);
    assert_eq!(summary.removed, 0);
    assert!(
        lease.workspace_path.exists(),
        "uuid dir preserved by disk GC"
    );

    let _ = cleanup_workspace_path(source.path(), &lease.workspace_path).await;
}

/// reconcile_disk_workspaces: removes a closed-issue workspace.
#[tokio::test]
async fn reconcile_disk_removes_closed_issue_workspace() {
    let _env_guard = async_env_lock().lock().await;
    let source = tempfile::tempdir().expect("tempdir");
    init_git_repo(source.path());

    let workspaces = tempfile::tempdir().expect("tempdir");
    let config = WorkspaceConfig {
        root: workspaces.path().to_path_buf(),
        ..Default::default()
    };
    let mgr = WorkspaceManager::new(config).expect("mgr");

    let issue_dir = workspaces.path().join("myorg_my-repo__issue_42");
    std::fs::create_dir_all(issue_dir.join(".git")).expect("mkdir");
    std::fs::write(
        issue_dir.join(".git").join(OWNER_RECORD_FILE),
        serde_json::to_vec(&WorkspaceOwnerRecord {
            task_id: "issue:42".to_string(),
            run_generation: 1,
            owner_session: "s".to_string(),
            workspace_key: Some("myorg_my-repo__issue_42".to_string()),
        })
        .expect("serialize"),
    )
    .expect("write record");

    let api_base =
        github_state_server("/repos/myorg/my-repo/issues/42", r#"{"state":"closed"}"#).await;
    let _api_base_guard = ScopedEnvVar::set("HARNESS_GITHUB_API_BASE_URL", &api_base);

    let summary = mgr
        .reconcile_disk_workspaces(source.path(), "gh", 20, None)
        .await;

    assert_eq!(summary.removed, 1);
    assert!(!issue_dir.exists(), "closed issue workspace removed");
}

#[tokio::test]
async fn reconcile_disk_removes_closed_issue_pool_slot_workspace() {
    let _env_guard = async_env_lock().lock().await;
    let source = tempfile::tempdir().expect("tempdir");
    init_git_repo(source.path());
    let branch = current_branch(source.path());

    let workspaces = tempfile::tempdir().expect("tempdir");
    let config = WorkspaceConfig {
        root: workspaces.path().to_path_buf(),
        ..Default::default()
    };
    let mgr = WorkspaceManager::new(config).expect("mgr");
    let task_id = harness_core::types::TaskId("closed-issue-pool-slot".to_string());

    let lease = mgr
        .create_workspace(
            &task_id,
            source.path(),
            "origin",
            &branch,
            1,
            Some("issue:42"),
            Some("myorg/my-repo"),
        )
        .await
        .expect("create pool slot workspace");
    mgr.release_workspace(&task_id).await;

    let owner = read_owner_record(&lease.workspace_path).expect("owner record");
    assert!(
        owner
            .workspace_key
            .as_deref()
            .and_then(repo_slug_from_workspace_key)
            .is_some(),
        "owner record should retain parseable repo identity"
    );
    let api_base =
        github_state_server("/repos/myorg/my-repo/issues/42", r#"{"state":"closed"}"#).await;
    let _api_base_guard = ScopedEnvVar::set("HARNESS_GITHUB_API_BASE_URL", &api_base);

    let summary = mgr
        .reconcile_disk_workspaces(source.path(), "gh", 20, None)
        .await;

    assert_eq!(summary.removed, 1);
    assert!(
        !lease.workspace_path.exists(),
        "closed issue pool slot workspace removed"
    );
}

#[tokio::test]
async fn reconcile_disk_skips_live_persisted_lease_from_other_manager() -> anyhow::Result<()> {
    let _env_guard = async_env_lock().lock().await;
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
    let task_id = harness_core::types::TaskId("live-closed-issue-slot".to_string());

    let lease = mgr_a
        .create_workspace(
            &task_id,
            source.path(),
            "origin",
            &branch,
            1,
            Some("issue:42"),
            Some("myorg/my-repo"),
        )
        .await
        .expect("create pool slot workspace");

    let api_base =
        github_state_server("/repos/myorg/my-repo/issues/42", r#"{"state":"closed"}"#).await;
    let _api_base_guard = ScopedEnvVar::set("HARNESS_GITHUB_API_BASE_URL", &api_base);

    let summary = mgr_b
        .reconcile_disk_workspaces(source.path(), "gh", 20, None)
        .await;

    assert_eq!(summary.removed, 0);
    assert_eq!(summary.skipped_open, 1);
    assert_eq!(summary.skipped_live.len(), 1);
    assert_eq!(summary.skipped_live[0].path, lease.workspace_path);
    assert_eq!(summary.skipped_live[0].task_id, task_id);
    assert_eq!(summary.skipped_live[0].owner_session, lease.owner_session);
    assert!(
        lease.workspace_path.exists(),
        "disk GC must not remove a workspace with a persisted live lease from another manager"
    );

    mgr_a.remove_workspace(&task_id).await?;
    Ok(())
}

#[tokio::test]
async fn reconcile_disk_releases_dead_persisted_lease_before_cleanup() -> anyhow::Result<()> {
    let _env_guard = async_env_lock().lock().await;
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
    let creator = WorkspaceManager::new(config.clone())?;
    let reconciler = WorkspaceManager::new_with_pool(
        config,
        WorkspacePoolConfig::default(),
        Some(store.clone()),
    )?;
    let task_id = harness_core::types::TaskId("dead-closed-issue-slot".to_string());

    let lease = creator
        .create_workspace(
            &task_id,
            source.path(),
            "origin",
            &branch,
            1,
            Some("issue:42"),
            Some("myorg/my-repo"),
        )
        .await?;
    creator.release_workspace(&task_id).await;

    let owner = read_owner_record(&lease.workspace_path).expect("owner record");
    let stale_record = WorkspaceLeaseRecord {
        project_key: "dead-project".to_string(),
        slot_index: 0,
        task_id: task_id.clone(),
        workspace_key: owner.workspace_key.expect("owner workspace key"),
        workspace_path: lease.workspace_path.clone(),
        source_repo: source.path().to_path_buf(),
        repo: Some("myorg/my-repo".to_string()),
        runtime_workflow_id: Some("workflow-dead".to_string()),
        owner_session: "dead-foreign-session".to_string(),
        run_generation: 1,
        process_id: u32::MAX,
        process_started_at: 1,
    };
    assert!(store.try_acquire_lease(&stale_record).await?);

    let api_base =
        github_state_server("/repos/myorg/my-repo/issues/42", r#"{"state":"closed"}"#).await;
    let _api_base_guard = ScopedEnvVar::set("HARNESS_GITHUB_API_BASE_URL", &api_base);

    let summary = reconciler
        .reconcile_disk_workspaces(source.path(), "gh", 20, None)
        .await;

    assert_eq!(summary.released_leases, 1);
    assert_eq!(summary.removed, 1);
    assert!(
        !lease.workspace_path.exists(),
        "closed issue workspace with dead persisted lease should be removed"
    );
    assert!(
        store.list_leased().await?.is_empty(),
        "dead persisted lease should be released before disk GC preservation checks"
    );

    Ok(())
}

#[tokio::test]
async fn reclaim_gate_skips_live_persisted_lease() -> anyhow::Result<()> {
    let _env_guard = async_env_lock().lock().await;
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
    let mgr = WorkspaceManager::new_with_pool(
        config,
        WorkspacePoolConfig::default(),
        Some(store.clone()),
    )?;
    let task_id = harness_core::types::TaskId("reclaim-gate-live-task".to_string());

    let lease = mgr
        .create_workspace(
            &task_id,
            source.path(),
            "origin",
            &branch,
            1,
            Some("issue:42"),
            Some("myorg/my-repo"),
        )
        .await?;

    let outcome =
        try_reclaim_workspace(source.path(), &lease.workspace_path, Some(&store), None).await?;

    assert_eq!(
        outcome,
        WorkspaceReclaimOutcome::SkippedLiveLease {
            task_id: task_id.clone(),
            owner_session: lease.owner_session.clone(),
        }
    );
    assert!(
        lease.workspace_path.exists(),
        "reclaim gate must preserve live leased workspaces"
    );

    mgr.remove_workspace(&task_id).await?;
    Ok(())
}

#[tokio::test]
async fn cleanup_orphan_worktrees_reclaim_gate_skips_live_persisted_lease() -> anyhow::Result<()> {
    let _env_guard = async_env_lock().lock().await;
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
    let task_id = harness_core::types::TaskId("live-terminal-orphan-task".to_string());

    let lease = mgr_a
        .create_workspace(&task_id, source.path(), "origin", &branch, 1, None, None)
        .await?;

    mgr_b
        .cleanup_orphan_worktrees(source.path(), std::slice::from_ref(&task_id))
        .await;

    assert!(
        lease.workspace_path.exists(),
        "orphan cleanup must not remove a workspace with a persisted live lease from another manager"
    );

    mgr_a.remove_workspace(&task_id).await?;
    Ok(())
}

#[tokio::test]
async fn reclaim_gate_deletes_workspace_without_live_lease() -> anyhow::Result<()> {
    let source = tempfile::tempdir()?;
    init_git_repo(source.path());
    let workspace = tempfile::tempdir()?;
    let workspace_path = workspace.path().join("dead-workspace");
    std::fs::create_dir_all(&workspace_path)?;

    let outcome = try_reclaim_workspace(source.path(), &workspace_path, None, Some(false)).await?;

    assert_eq!(outcome, WorkspaceReclaimOutcome::Deleted);
    assert!(
        !workspace_path.exists(),
        "reclaim gate must delete unleased workspaces"
    );
    Ok(())
}

/// reconcile_disk_workspaces: preserves an open-issue workspace.
#[tokio::test]
async fn reconcile_disk_skips_open_issue_workspace() {
    let _env_guard = async_env_lock().lock().await;
    let source = tempfile::tempdir().expect("tempdir");
    init_git_repo(source.path());

    let workspaces = tempfile::tempdir().expect("tempdir");
    let config = WorkspaceConfig {
        root: workspaces.path().to_path_buf(),
        ..Default::default()
    };
    let mgr = WorkspaceManager::new(config).expect("mgr");

    let issue_dir = workspaces.path().join("myorg_my-repo__issue_7");
    std::fs::create_dir_all(issue_dir.join(".git")).expect("mkdir");
    std::fs::write(
        issue_dir.join(".git").join(OWNER_RECORD_FILE),
        serde_json::to_vec(&WorkspaceOwnerRecord {
            task_id: "issue:7".to_string(),
            run_generation: 1,
            owner_session: "s".to_string(),
            workspace_key: Some("myorg_my-repo__issue_7".to_string()),
        })
        .expect("serialize"),
    )
    .expect("write record");

    let api_base =
        github_state_server("/repos/myorg/my-repo/issues/7", r#"{"state":"open"}"#).await;
    let _api_base_guard = ScopedEnvVar::set("HARNESS_GITHUB_API_BASE_URL", &api_base);

    let summary = mgr
        .reconcile_disk_workspaces(source.path(), "gh", 20, None)
        .await;

    assert_eq!(summary.removed, 0);
    assert_eq!(summary.skipped_open, 1);
    assert!(issue_dir.exists(), "open issue workspace preserved");
}
