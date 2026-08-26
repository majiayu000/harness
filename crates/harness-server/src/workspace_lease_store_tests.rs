use super::test_support::*;
use super::*;
use harness_core::db::TestSchemaGuard;

#[tokio::test]
async fn workspace_lease_store_persists_and_releases_active_slots() -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let store = WorkspaceLeaseStore::open(&dir.path().join("workspace-leases")).await?;
    let task_id = harness_core::types::TaskId("lease-store-task".to_string());
    let process_started_at = WorkspaceLeaseStore::current_process_started_at()?;
    let record = WorkspaceLeaseRecord {
        project_key: "project-a".to_string(),
        slot_index: 0,
        task_id: task_id.clone(),
        workspace_key: "workspace-a".to_string(),
        workspace_path: dir.path().join("workspaces/project-a-slot-0"),
        source_repo: dir.path().join("repo"),
        repo: Some("owner/repo".to_string()),
        runtime_workflow_id: Some("workflow-1".to_string()),
        owner_session: "session-a".to_string(),
        run_generation: 1,
        acquisition_id: Some("acquisition-a".to_string()),
        process_id: std::process::id(),
        process_started_at,
    };

    assert!(
        store.try_acquire_lease(&record).await?,
        "initial lease should acquire an empty slot"
    );
    assert_eq!(store.list_leased().await?.len(), 1);
    assert_eq!(
        store.latest_workspace_path_for_task(&task_id).await?,
        Some(record.workspace_path.clone())
    );
    assert!(
        store
            .release_owned_slot(
                &record.project_key,
                record.slot_index,
                &task_id,
                &record.owner_session,
                record.run_generation,
                record.acquisition_id.as_deref().expect("acquisition ID"),
            )
            .await?,
        "release should update the active lease"
    );
    assert!(store.list_leased().await?.is_empty());

    Ok(())
}

#[tokio::test]
async fn durable_workspace_completion_is_idempotent_for_exact_acquisition() -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }
    let dir = tempfile::tempdir()?;
    let store = WorkspaceLeaseStore::open(&dir.path().join("idempotent-completion")).await?;
    let record = WorkspaceLeaseRecord {
        project_key: "idempotent-project".to_string(),
        slot_index: 0,
        task_id: TaskId::from_str("idempotent-task"),
        workspace_key: "idempotent-workspace".to_string(),
        workspace_path: dir.path().join("workspaces/idempotent"),
        source_repo: dir.path().join("repo"),
        repo: Some("owner/repo".to_string()),
        runtime_workflow_id: Some("idempotent-workflow".to_string()),
        owner_session: "idempotent-session".to_string(),
        run_generation: 1,
        acquisition_id: Some("idempotent-acquisition-a".to_string()),
        process_id: std::process::id(),
        process_started_at: WorkspaceLeaseStore::current_process_started_at()?,
    };
    assert!(store.try_acquire_lease(&record).await?);
    let acquisition_id = record.acquisition_id.as_deref().expect("acquisition ID");

    for _ in 0..2 {
        store
            .complete_owned_workspace(
                &record.project_key,
                record.slot_index,
                &record.task_id,
                &record.owner_session,
                record.run_generation,
                acquisition_id,
            )
            .await?;
    }

    let replacement = WorkspaceLeaseRecord {
        acquisition_id: Some("idempotent-acquisition-b".to_string()),
        ..record.clone()
    };
    assert!(store.try_acquire_lease(&replacement).await?);
    let stale_error = store
        .complete_owned_workspace(
            &record.project_key,
            record.slot_index,
            &record.task_id,
            &record.owner_session,
            record.run_generation,
            acquisition_id,
        )
        .await
        .expect_err("a replaced acquisition must remain fenced");
    assert!(stale_error
        .to_string()
        .contains("workspace acquisition changed"));
    Ok(())
}

#[tokio::test]
async fn acquisition_token_fences_stale_release_and_cleanup() -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }
    let dir = tempfile::tempdir()?;
    let store = WorkspaceLeaseStore::open(&dir.path().join("acquisition-fence")).await?;
    let task_id = TaskId::from_str("acquisition-fence-task");
    let record_a = WorkspaceLeaseRecord {
        project_key: "project-a".to_string(),
        slot_index: 0,
        task_id: task_id.clone(),
        workspace_key: "workspace-a".to_string(),
        workspace_path: dir.path().join("workspaces/a"),
        source_repo: dir.path().join("repo"),
        repo: Some("owner/repo".to_string()),
        runtime_workflow_id: Some("workflow-a".to_string()),
        owner_session: "session-a".to_string(),
        run_generation: 1,
        acquisition_id: Some("acquisition-a".to_string()),
        process_id: std::process::id(),
        process_started_at: WorkspaceLeaseStore::current_process_started_at()?,
    };
    assert!(store.try_acquire_lease(&record_a).await?);
    assert!(
        store
            .owned_workspace_acquisition_is_current(
                &record_a.project_key,
                record_a.slot_index,
                &task_id,
                &record_a.workspace_path,
                &record_a.owner_session,
                record_a.run_generation,
                "acquisition-a",
            )
            .await?,
        "the exact durable acquisition should be current before replacement"
    );
    let target_a = store
        .workspace_cleanup_targets_for_runtime_workflow("workflow-a")
        .await?
        .pop()
        .expect("target A");
    let record_b = WorkspaceLeaseRecord {
        acquisition_id: Some("acquisition-b".to_string()),
        ..record_a.clone()
    };
    assert!(store.try_acquire_lease(&record_b).await?);
    assert!(
        !store
            .owned_workspace_acquisition_is_current(
                &record_a.project_key,
                record_a.slot_index,
                &task_id,
                &record_a.workspace_path,
                &record_a.owner_session,
                record_a.run_generation,
                "acquisition-a",
            )
            .await?,
        "a replaced acquisition must fail the detached-cleanup fence"
    );
    assert!(
        store
            .owned_workspace_acquisition_is_current(
                &record_b.project_key,
                record_b.slot_index,
                &task_id,
                &record_b.workspace_path,
                &record_b.owner_session,
                record_b.run_generation,
                "acquisition-b",
            )
            .await?,
        "the replacement acquisition should pass the detached-cleanup fence"
    );

    assert!(
        !store
            .release_owned_slot(
                &record_a.project_key,
                record_a.slot_index,
                &task_id,
                &record_a.owner_session,
                record_a.run_generation,
                "acquisition-a",
            )
            .await?,
        "stale acquisition must not release its replacement"
    );
    store.complete_workspace_cleanup_target(&target_a).await?;
    assert_eq!(store.list_leased().await?, vec![record_b.clone()]);
    let targets = store
        .workspace_cleanup_targets_for_runtime_workflow("workflow-a")
        .await?;
    assert_eq!(targets.len(), 1);
    assert_eq!(targets[0].acquisition_id.as_deref(), Some("acquisition-b"));
    Ok(())
}

#[tokio::test]
async fn cleanup_claim_blocks_slot_reuse_until_physical_cleanup_completes() -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }
    let dir = tempfile::tempdir()?;
    let store = WorkspaceLeaseStore::open(&dir.path().join("cleanup-claim")).await?;
    let record_a = WorkspaceLeaseRecord {
        project_key: "cleanup-project".to_string(),
        slot_index: 0,
        task_id: TaskId::from_str("cleanup-task-a"),
        workspace_key: "cleanup-workspace".to_string(),
        workspace_path: dir.path().join("workspaces/cleanup"),
        source_repo: dir.path().join("repo"),
        repo: Some("owner/repo".to_string()),
        runtime_workflow_id: Some("cleanup-workflow".to_string()),
        owner_session: "cleanup-session-a".to_string(),
        run_generation: 1,
        acquisition_id: Some("cleanup-acquisition-a".to_string()),
        process_id: std::process::id(),
        process_started_at: WorkspaceLeaseStore::current_process_started_at()?,
    };
    assert!(store.try_acquire_lease(&record_a).await?);
    assert!(
        store
            .release_owned_slot(
                &record_a.project_key,
                record_a.slot_index,
                &record_a.task_id,
                &record_a.owner_session,
                record_a.run_generation,
                record_a.acquisition_id.as_deref().expect("acquisition A"),
            )
            .await?
    );
    let target = store
        .workspace_cleanup_targets_for_runtime_workflow("cleanup-workflow")
        .await?
        .pop()
        .expect("cleanup target");
    assert_eq!(
        store
            .claim_workspace_cleanup_hook(
                "cleanup-workflow",
                &record_a.workspace_path,
                WorkspaceCleanupHook::Workflow,
            )
            .await?,
        Some(true)
    );
    assert_eq!(
        store
            .claim_workspace_cleanup_hook(
                "cleanup-workflow",
                &record_a.workspace_path,
                WorkspaceCleanupHook::Workflow,
            )
            .await?,
        Some(false)
    );
    let reopened = WorkspaceLeaseStore::open(&dir.path().join("cleanup-claim")).await?;
    assert_eq!(
        reopened
            .claim_workspace_cleanup_hook(
                "cleanup-workflow",
                &record_a.workspace_path,
                WorkspaceCleanupHook::Workflow,
            )
            .await?,
        Some(false)
    );
    assert_eq!(
        reopened
            .claim_workspace_cleanup_hook(
                "cleanup-workflow",
                &record_a.workspace_path,
                WorkspaceCleanupHook::Manager,
            )
            .await?,
        Some(true)
    );
    assert_eq!(
        store
            .claim_workspace_cleanup_hook(
                "cleanup-workflow",
                &record_a.workspace_path,
                WorkspaceCleanupHook::Manager,
            )
            .await?,
        Some(false)
    );
    let cleanup_claim_id = "cleanup-claim-a";
    assert!(
        store
            .claim_workspace_cleanup_target(
                &target,
                cleanup_claim_id,
                "cleanup-worker-a",
                std::process::id(),
                WorkspaceLeaseStore::current_process_started_at()?,
            )
            .await?
    );

    let record_b = WorkspaceLeaseRecord {
        task_id: TaskId::from_str("cleanup-task-b"),
        owner_session: "cleanup-session-b".to_string(),
        acquisition_id: Some("cleanup-acquisition-b".to_string()),
        ..record_a.clone()
    };
    assert!(
        !store.try_acquire_lease(&record_b).await?,
        "a cleaning slot must not be reused"
    );
    assert!(
        !store
            .claim_workspace_cleanup_target(
                &target,
                "cleanup-claim-b",
                "cleanup-worker-b",
                std::process::id(),
                WorkspaceLeaseStore::current_process_started_at()?,
            )
            .await?,
        "a live cleaner must retain its exact claim"
    );
    assert!(
        store
            .expire_workspace_cleanup_claim_for_test(&target, cleanup_claim_id, "cleanup-worker-a",)
            .await?
    );
    let replacement_claim_id = "cleanup-claim-b";
    assert!(
        store
            .claim_workspace_cleanup_target(
                &target,
                replacement_claim_id,
                "cleanup-worker-b",
                std::process::id(),
                WorkspaceLeaseStore::current_process_started_at()?,
            )
            .await?,
        "an expired cleanup task claim must be recoverable even while its process remains live"
    );
    assert!(
        store
            .abandon_workspace_cleanup_claim(&target, replacement_claim_id, "cleanup-worker-b",)
            .await?
    );
    let final_claim_id = "cleanup-claim-c";
    assert!(
        store
            .claim_workspace_cleanup_target(
                &target,
                final_claim_id,
                "cleanup-worker-c",
                std::process::id(),
                WorkspaceLeaseStore::current_process_started_at()?,
            )
            .await?
    );

    store
        .complete_claimed_workspace_cleanup_target(&target, final_claim_id, "cleanup-worker-c")
        .await?;
    assert!(store.try_acquire_lease(&record_b).await?);
    Ok(())
}

#[tokio::test]
async fn cancelled_persisted_acquisition_preserves_cleanup_target() -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }
    let source = tempfile::tempdir()?;
    init_git_repo(source.path());
    let branch = current_branch(source.path());
    let workspaces = tempfile::tempdir()?;
    let lease_db = tempfile::tempdir()?;
    let store = std::sync::Arc::new(
        WorkspaceLeaseStore::open(&lease_db.path().join("cancelled-acquisition")).await?,
    );
    let manager = std::sync::Arc::new(WorkspaceManager::new_with_pool(
        WorkspaceConfig {
            root: workspaces.path().to_path_buf(),
            ..Default::default()
        },
        WorkspacePoolConfig::new(1, std::collections::HashMap::new()),
        Some(store.clone()),
    )?);
    let task_id = TaskId::from_str("cancelled-acquisition-task");
    let first = manager
        .create_workspace_with_options(
            &task_id,
            source.path(),
            "origin",
            &branch,
            1,
            Some("issue:cancelled-acquisition"),
            Some("owner/repo"),
            WorkspaceCreateOptions {
                require_remote_head: false,
                runtime_workflow_id: Some("cancelled-acquisition-workflow".to_string()),
                ..Default::default()
            },
        )
        .await?;
    manager.release_workspace(&task_id).await;

    let git_ops_guard = manager.git_ops.lock().await;
    let manager_for_create = manager.clone();
    let task_for_create = task_id.clone();
    let source_path = source.path().to_path_buf();
    let branch_for_create = branch.clone();
    let create = tokio::spawn(async move {
        manager_for_create
            .create_workspace_with_options(
                &task_for_create,
                &source_path,
                "origin",
                &branch_for_create,
                1,
                Some("issue:cancelled-acquisition"),
                Some("owner/repo"),
                WorkspaceCreateOptions {
                    require_remote_head: false,
                    runtime_workflow_id: Some("cancelled-acquisition-workflow".to_string()),
                    persist_runtime_cleanup_target: false,
                    ..Default::default()
                },
            )
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while store
            .list_leased()
            .await
            .map_or(true, |leases| leases.is_empty())
        {
            tokio::task::yield_now().await;
        }
    })
    .await?;

    create.abort();
    drop(git_ops_guard);
    assert!(create
        .await
        .expect_err("creation should be cancelled")
        .is_cancelled());
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while store
            .list_leased()
            .await
            .map_or(true, |leases| !leases.is_empty())
        {
            tokio::task::yield_now().await;
        }
    })
    .await?;

    assert!(first.workspace_path.exists());
    let targets = store
        .workspace_cleanup_targets_for_runtime_workflow("cancelled-acquisition-workflow")
        .await?;
    assert_eq!(targets.len(), 1);
    assert_eq!(
        targets[0].acquisition_id.as_deref(),
        Some(first.acquisition_id.as_str())
    );
    Ok(())
}

#[tokio::test]
async fn after_run_workspace_lease_does_not_persist_terminal_cleanup_target() -> anyhow::Result<()>
{
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }
    let dir = tempfile::tempdir()?;
    let store = WorkspaceLeaseStore::open(&dir.path().join("after-run-cleanup-target")).await?;
    let record = WorkspaceLeaseRecord {
        project_key: "project-after-run".to_string(),
        slot_index: 0,
        task_id: harness_core::types::TaskId("after-run-task".to_string()),
        workspace_key: "workspace-after-run".to_string(),
        workspace_path: dir.path().join("workspaces/after-run"),
        source_repo: dir.path().join("repo"),
        repo: Some("owner/repo".to_string()),
        runtime_workflow_id: Some("workflow-after-run".to_string()),
        owner_session: "session-after-run".to_string(),
        run_generation: 1,
        acquisition_id: Some("acquisition-after-run".to_string()),
        process_id: std::process::id(),
        process_started_at: WorkspaceLeaseStore::current_process_started_at()?,
    };

    assert!(store.try_acquire_lease(&record).await?);
    assert_eq!(
        store
            .workspace_cleanup_targets_for_runtime_workflow("workflow-after-run")
            .await?
            .len(),
        1
    );
    assert!(
        store
            .try_acquire_lease_with_cleanup_target(&record, false)
            .await?
    );
    assert!(store
        .workspace_cleanup_targets_for_runtime_workflow("workflow-after-run")
        .await?
        .is_empty());
    Ok(())
}

#[tokio::test]
async fn runtime_cleanup_releases_only_the_exact_candidate_lease() -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }
    let dir = tempfile::tempdir()?;
    let store = std::sync::Arc::new(
        WorkspaceLeaseStore::open(&dir.path().join("exact-candidate-release")).await?,
    );
    let manager = WorkspaceManager::new_with_pool(
        WorkspaceConfig {
            root: dir.path().join("workspaces"),
            ..Default::default()
        },
        WorkspacePoolConfig::new(2, std::collections::HashMap::new()),
        Some(store.clone()),
    )?;
    let process_started_at = WorkspaceLeaseStore::current_process_started_at()?;
    let shared_task_id = harness_core::types::TaskId("issue-1300-c1".to_string());
    let record_a = WorkspaceLeaseRecord {
        project_key: "/repo/a".to_string(),
        slot_index: 0,
        task_id: shared_task_id.clone(),
        workspace_key: "repo-a-issue-1300".to_string(),
        workspace_path: dir.path().join("workspaces/repo-a-slot-0"),
        source_repo: dir.path().join("repo-a"),
        repo: Some("owner/repo-a".to_string()),
        runtime_workflow_id: Some("workflow-a".to_string()),
        owner_session: "session-a".to_string(),
        run_generation: 1,
        acquisition_id: Some("acquisition-a".to_string()),
        process_id: std::process::id(),
        process_started_at,
    };
    let record_b = WorkspaceLeaseRecord {
        project_key: "/repo/b".to_string(),
        workspace_key: "repo-b-issue-1300".to_string(),
        workspace_path: dir.path().join("workspaces/repo-b-slot-0"),
        source_repo: dir.path().join("repo-b"),
        repo: Some("owner/repo-b".to_string()),
        runtime_workflow_id: Some("workflow-b".to_string()),
        owner_session: "session-b".to_string(),
        ..record_a.clone()
    };
    assert!(store.try_acquire_lease(&record_a).await?);
    assert!(store.try_acquire_lease(&record_b).await?);
    assert_eq!(
        store
            .runtime_workspace_cleanup_workflow_ids_after(None, 10)
            .await?,
        vec!["workflow-a".to_string(), "workflow-b".to_string()]
    );
    assert_eq!(
        store
            .runtime_workspace_cleanup_workflow_ids_after(Some("workflow-a"), 10)
            .await?,
        vec!["workflow-b".to_string()]
    );

    let stale_targets = manager
        .workspace_targets_for_runtime_workflow("workflow-a")
        .await?;
    assert_eq!(stale_targets.len(), 1);
    let mut refreshed_record_a = record_a.clone();
    refreshed_record_a.run_generation = 2;
    assert!(store.try_acquire_lease(&refreshed_record_a).await?);
    manager
        .release_runtime_workspace_cleanup_target(&stale_targets[0])
        .await?;
    let refreshed_targets = store
        .workspace_cleanup_targets_for_runtime_workflow("workflow-a")
        .await?;
    assert_eq!(refreshed_targets.len(), 1);
    assert_eq!(refreshed_targets[0].run_generation, 2);
    let current_targets = manager
        .workspace_targets_for_runtime_workflow("workflow-a")
        .await?;
    manager
        .release_runtime_workspace_cleanup_target(&current_targets[0])
        .await?;

    let leased = store.list_leased().await?;
    assert_eq!(leased.len(), 1);
    assert_eq!(leased[0].project_key, record_b.project_key);
    assert_eq!(leased[0].task_id, shared_task_id);
    Ok(())
}

#[tokio::test]
async fn missing_runtime_workflow_cleanup_uses_durable_target_and_fails_closed(
) -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }
    let source = tempfile::tempdir()?;
    init_git_repo(source.path());
    let workspaces = tempfile::tempdir()?;
    let lease_db = tempfile::tempdir()?;
    let store = std::sync::Arc::new(
        WorkspaceLeaseStore::open(&lease_db.path().join("missing-workflow-cleanup")).await?,
    );
    let manager = WorkspaceManager::new_with_pool(
        WorkspaceConfig {
            root: workspaces.path().to_path_buf(),
            ..Default::default()
        },
        WorkspacePoolConfig::new(2, std::collections::HashMap::new()),
        Some(store.clone()),
    )?;
    let workspace_path = workspaces.path().join("orphaned-runtime-workspace");
    std::fs::create_dir_all(&workspace_path)?;
    let record = WorkspaceLeaseRecord {
        project_key: crate::workspace_pool::project_limit_key(source.path()),
        slot_index: 0,
        task_id: TaskId::from_str("missing-workflow-task"),
        workspace_key: "missing-workflow-workspace".to_string(),
        workspace_path: workspace_path.clone(),
        source_repo: source.path().to_path_buf(),
        repo: Some("owner/repo".to_string()),
        runtime_workflow_id: Some("missing-workflow".to_string()),
        owner_session: "missing-workflow-session".to_string(),
        run_generation: 1,
        acquisition_id: Some("acquisition-missing".to_string()),
        process_id: std::process::id(),
        process_started_at: WorkspaceLeaseStore::current_process_started_at()?,
    };
    assert!(store.try_acquire_lease(&record).await?);

    assert_eq!(
        manager
            .cleanup_missing_runtime_workflow_targets_if_uncontended("missing-workflow")
            .await?,
        1
    );
    assert!(!workspace_path.exists());
    assert!(store
        .workspace_cleanup_targets_for_runtime_workflow("missing-workflow")
        .await?
        .is_empty());

    let outside = tempfile::tempdir()?;
    let mut unsafe_record = record;
    unsafe_record.workspace_path = outside.path().join("must-remain");
    unsafe_record.runtime_workflow_id = Some("unsafe-missing-workflow".to_string());
    std::fs::create_dir_all(&unsafe_record.workspace_path)?;
    assert!(store.try_acquire_lease(&unsafe_record).await?);
    let mut safe_followup = unsafe_record.clone();
    safe_followup.slot_index = 1;
    safe_followup.task_id = TaskId::from_str("safe-followup-task");
    safe_followup.workspace_key = "safe-followup-workspace".to_string();
    safe_followup.workspace_path = workspaces.path().join("safe-followup-workspace");
    std::fs::create_dir_all(&safe_followup.workspace_path)?;
    assert!(store.try_acquire_lease(&safe_followup).await?);
    let error = manager
        .cleanup_missing_runtime_workflow_targets_if_uncontended("unsafe-missing-workflow")
        .await
        .expect_err("cleanup target outside the workspace root must be rejected");
    assert!(error.to_string().contains("outside configured root"));
    assert!(unsafe_record.workspace_path.exists());
    assert!(!safe_followup.workspace_path.exists());
    let remaining = store
        .workspace_cleanup_targets_for_runtime_workflow("unsafe-missing-workflow")
        .await?;
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].workspace_path, unsafe_record.workspace_path);
    Ok(())
}

#[tokio::test]
async fn runtime_cleanup_does_not_remove_same_task_id_from_another_repository() -> anyhow::Result<()>
{
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }
    let repo_a = tempfile::tempdir()?;
    let repo_b = tempfile::tempdir()?;
    init_git_repo(repo_a.path());
    init_git_repo(repo_b.path());
    let branch_a = current_branch(repo_a.path());
    let branch_b = current_branch(repo_b.path());
    let workspaces = tempfile::tempdir()?;
    let lease_db = tempfile::tempdir()?;
    let store = std::sync::Arc::new(
        WorkspaceLeaseStore::open(&lease_db.path().join("cross-repository-task-id")).await?,
    );
    let config = WorkspaceConfig {
        root: workspaces.path().to_path_buf(),
        ..Default::default()
    };
    let pool_config = WorkspacePoolConfig::new(2, std::collections::HashMap::new());
    let manager_a =
        WorkspaceManager::new_with_pool(config.clone(), pool_config.clone(), Some(store.clone()))?;
    let manager_b = WorkspaceManager::new_with_pool(config, pool_config, Some(store.clone()))?;
    let task_id = harness_core::types::TaskId("issue-1300-c1".to_string());
    let workspace_a = manager_a
        .create_workspace_with_options(
            &task_id,
            repo_a.path(),
            "origin",
            &branch_a,
            1,
            Some("issue:1300"),
            Some("owner/repo-a"),
            WorkspaceCreateOptions {
                require_remote_head: false,
                runtime_workflow_id: Some("workflow-a".to_string()),
                ..Default::default()
            },
        )
        .await?;
    manager_a.release_workspace(&task_id).await;
    let workspace_b = manager_b
        .create_workspace_with_options(
            &task_id,
            repo_b.path(),
            "origin",
            &branch_b,
            1,
            Some("issue:1300"),
            Some("owner/repo-b"),
            WorkspaceCreateOptions {
                require_remote_head: false,
                runtime_workflow_id: Some("workflow-b".to_string()),
                ..Default::default()
            },
        )
        .await?;

    let _repository_lease = manager_b
        .acquire_repository_write_lease_for_cleanup(repo_a.path())
        .await?;
    manager_b
        .cleanup_workspace_for_retry(&task_id, repo_a.path(), Some(&workspace_a.workspace_path))
        .await?;
    let targets = manager_b
        .workspace_targets_for_runtime_workflow("workflow-a")
        .await?;
    assert_eq!(targets.len(), 1);
    manager_b
        .release_runtime_workspace_cleanup_target(&targets[0])
        .await?;

    assert!(!workspace_a.workspace_path.exists());
    assert!(workspace_b.workspace_path.exists());
    assert!(manager_b.active.contains_key(&task_id));
    let leased = store.list_leased().await?;
    assert_eq!(leased.len(), 1);
    assert_eq!(leased[0].runtime_workflow_id.as_deref(), Some("workflow-b"));
    manager_b.remove_workspace(&task_id).await?;
    Ok(())
}

#[tokio::test]
async fn runtime_cleanup_target_survives_slot_reuse_with_a_different_repo_slug(
) -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }
    let source_repo = tempfile::tempdir()?;
    init_git_repo(source_repo.path());
    let branch = current_branch(source_repo.path());
    let workspaces = tempfile::tempdir()?;
    let lease_db = tempfile::tempdir()?;
    let store = std::sync::Arc::new(
        WorkspaceLeaseStore::open(&lease_db.path().join("slot-reuse-cleanup-history")).await?,
    );
    let config = WorkspaceConfig {
        root: workspaces.path().to_path_buf(),
        ..Default::default()
    };
    let pool_config = WorkspacePoolConfig::new(1, std::collections::HashMap::new());
    let manager_a =
        WorkspaceManager::new_with_pool(config.clone(), pool_config.clone(), Some(store.clone()))?;
    let manager_b = WorkspaceManager::new_with_pool(config, pool_config, Some(store.clone()))?;
    let task_a = harness_core::types::TaskId("issue-1300-a".to_string());
    let task_b = harness_core::types::TaskId("issue-1300-b".to_string());

    let workspace_a = manager_a
        .create_workspace_with_options(
            &task_a,
            source_repo.path(),
            "origin",
            &branch,
            1,
            Some("issue:1300"),
            Some("owner/repo-a"),
            WorkspaceCreateOptions {
                require_remote_head: false,
                runtime_workflow_id: Some("workflow-a".to_string()),
                ..Default::default()
            },
        )
        .await?;
    manager_a.release_workspace(&task_a).await;

    let workspace_b = manager_b
        .create_workspace_with_options(
            &task_b,
            source_repo.path(),
            "origin",
            &branch,
            1,
            Some("issue:1300"),
            Some("owner/repo-b"),
            WorkspaceCreateOptions {
                require_remote_head: false,
                runtime_workflow_id: Some("workflow-b".to_string()),
                ..Default::default()
            },
        )
        .await?;
    assert_ne!(workspace_a.workspace_path, workspace_b.workspace_path);
    assert_eq!(
        manager_b.active.get(&task_b).and_then(|active| {
            active
                ._repository_write_lease
                .as_ref()
                .map(RepositoryWriteLease::mode)
        }),
        Some(RepositoryLeaseMode::Exclusive),
        "the active replacement workspace must protect cleanup with its exclusive repository lease"
    );
    let targets_a = manager_b
        .workspace_targets_for_runtime_workflow("workflow-a")
        .await?;
    assert_eq!(targets_a.len(), 1);
    assert_eq!(targets_a[0].workspace_path, workspace_a.workspace_path);
    assert!(
        !manager_b
            .runtime_workspace_cleanup_target_is_superseded(&targets_a[0])
            .await?
    );
    let cleanup_claim = manager_b
        .claim_runtime_workspace_cleanup_target(&targets_a[0])
        .await?
        .ok_or_else(|| {
            anyhow::anyhow!("historical path A must remain claimable after slot reuse by path B")
        })?;
    manager_b
        .cleanup_workspace_for_retry(
            &targets_a[0].task_id,
            source_repo.path(),
            Some(&targets_a[0].workspace_path),
        )
        .await?;
    cleanup_claim.complete(&manager_b, &targets_a[0]).await?;

    assert!(!workspace_a.workspace_path.exists());
    assert!(workspace_b.workspace_path.exists());
    assert!(manager_b
        .workspace_targets_for_runtime_workflow("workflow-a")
        .await?
        .is_empty());
    assert_eq!(
        manager_b
            .workspace_targets_for_runtime_workflow("workflow-b")
            .await?
            .len(),
        1
    );
    let current = store
        .current_workspace_lease_for_slot(
            &crate::workspace_pool::project_limit_key(source_repo.path()),
            0,
        )
        .await?
        .expect("workflow B should retain the current slot record");
    assert_eq!(current.runtime_workflow_id.as_deref(), Some("workflow-b"));
    assert_eq!(current.workspace_path, workspace_b.workspace_path);
    manager_b
        .remove_workspace_acquisition(&task_b, &workspace_b.acquisition_id)
        .await?;
    Ok(())
}

#[tokio::test]
async fn workspace_lease_store_shared_schema_keeps_data_dirs_isolated() -> anyhow::Result<()> {
    let database_url = match harness_core::db::resolve_test_database_url(None) {
        Ok(url) => url,
        Err(_) => return Ok(()),
    };
    let dir = tempfile::tempdir().expect("tempdir");
    let setup_pool = harness_core::db::pg_open_pool(&database_url).await?;
    let mut shared_schema = TestSchemaGuard::new(&database_url, "workspace_lease_scope_test")?;
    let shared_context =
        harness_core::db::PgStoreContext::from_schema(shared_schema.schema(), Some(&database_url))?;
    let store_a_dir = dir.path().join("store-a");
    let store_b_dir = dir.path().join("store-b");
    std::fs::create_dir_all(&store_a_dir)?;
    std::fs::create_dir_all(&store_b_dir)?;
    crate::task_db::TaskDb::open_shared_with_data_dir(&shared_context, &setup_pool, &store_a_dir)
        .await?;
    crate::task_db::TaskDb::open_shared_with_data_dir(&shared_context, &setup_pool, &store_b_dir)
        .await?;
    let store_a =
        WorkspaceLeaseStore::open_shared_with_data_dir(&shared_context, &setup_pool, &store_a_dir)
            .await?;
    let store_b =
        WorkspaceLeaseStore::open_shared_with_data_dir(&shared_context, &setup_pool, &store_b_dir)
            .await?;
    let process_started_at = WorkspaceLeaseStore::current_process_started_at()?;
    let record_a = WorkspaceLeaseRecord {
        project_key: "project-a".to_string(),
        slot_index: 0,
        task_id: harness_core::types::TaskId("store-a-task".to_string()),
        workspace_key: "workspace-a".to_string(),
        workspace_path: dir.path().join("workspaces/store-a"),
        source_repo: dir.path().join("repo-a"),
        repo: Some("owner/repo-a".to_string()),
        runtime_workflow_id: Some("workflow-a".to_string()),
        owner_session: "session-a".to_string(),
        run_generation: 1,
        acquisition_id: Some("acquisition-store-a".to_string()),
        process_id: std::process::id(),
        process_started_at,
    };
    let record_b = WorkspaceLeaseRecord {
        task_id: harness_core::types::TaskId("store-b-task".to_string()),
        workspace_key: "workspace-b".to_string(),
        workspace_path: dir.path().join("workspaces/store-b"),
        source_repo: dir.path().join("repo-b"),
        repo: Some("owner/repo-b".to_string()),
        runtime_workflow_id: Some("workflow-b".to_string()),
        owner_session: "session-b".to_string(),
        ..record_a.clone()
    };

    assert!(store_a.try_acquire_lease(&record_a).await?);
    assert!(
        store_b.try_acquire_lease(&record_b).await?,
        "same project slot should be isolated by store_key"
    );

    let leased_a = store_a.list_leased().await?;
    let leased_b = store_b.list_leased().await?;
    assert_eq!(leased_a.len(), 1);
    assert_eq!(leased_b.len(), 1);
    assert_eq!(leased_a[0].task_id, record_a.task_id);
    assert_eq!(leased_b[0].task_id, record_b.task_id);

    shared_schema.cleanup_with_pool(&setup_pool).await?;
    setup_pool.close().await;
    Ok(())
}

#[tokio::test]
async fn legacy_backfill_copies_runtime_workspace_cleanup_targets() -> anyhow::Result<()> {
    let database_url = match harness_core::db::resolve_test_database_url(None) {
        Ok(url) => url,
        Err(_) => return Ok(()),
    };
    let dir = tempfile::tempdir()?;
    let legacy_path = dir.path().join("legacy-task-db");
    let _legacy_db =
        crate::task_db::TaskDb::open_with_database_url(&legacy_path, Some(database_url.as_str()))
            .await?;
    let legacy_store = WorkspaceLeaseStore::open(&legacy_path).await?;
    let record = WorkspaceLeaseRecord {
        project_key: "/repo/legacy".to_string(),
        slot_index: 0,
        task_id: harness_core::types::TaskId("legacy-runtime-task".to_string()),
        workspace_key: "legacy-runtime-workspace".to_string(),
        workspace_path: dir.path().join("workspaces/legacy-runtime-slot-0"),
        source_repo: dir.path().join("repo"),
        repo: Some("owner/legacy-repo".to_string()),
        runtime_workflow_id: Some("legacy-runtime-workflow".to_string()),
        owner_session: "legacy-session".to_string(),
        run_generation: 1,
        acquisition_id: Some("acquisition-legacy".to_string()),
        process_id: std::process::id(),
        process_started_at: WorkspaceLeaseStore::current_process_started_at()?,
    };
    assert!(legacy_store.try_acquire_lease(&record).await?);
    assert_eq!(
        legacy_store
            .claim_workspace_cleanup_hook(
                "legacy-runtime-workflow",
                &record.workspace_path,
                WorkspaceCleanupHook::Workflow,
            )
            .await?,
        Some(true)
    );
    assert_eq!(
        legacy_store
            .claim_workspace_cleanup_hook(
                "legacy-runtime-workflow",
                &record.workspace_path,
                WorkspaceCleanupHook::Manager,
            )
            .await?,
        Some(true)
    );

    let setup_pool = harness_core::db::pg_open_pool(&database_url).await?;
    let mut shared_schema = TestSchemaGuard::new(&database_url, "workspace_lease_scope_test")?;
    let shared_context =
        harness_core::db::PgStoreContext::from_schema(shared_schema.schema(), Some(&database_url))?;
    let shared_data_dir = dir.path().join("shared-data");
    std::fs::create_dir_all(&shared_data_dir)?;
    let shared_db = crate::task_db::TaskDb::open_shared_with_data_dir(
        &shared_context,
        &setup_pool,
        &shared_data_dir,
    )
    .await?;
    crate::task_db::migrate_legacy_task_db_if_needed(
        &legacy_path,
        Some(database_url.as_str()),
        &shared_db,
    )
    .await?;
    let shared_store = WorkspaceLeaseStore::open_shared_with_data_dir(
        &shared_context,
        &setup_pool,
        &shared_data_dir,
    )
    .await?;

    let targets = shared_store
        .workspace_cleanup_targets_for_runtime_workflow("legacy-runtime-workflow")
        .await?;
    assert_eq!(targets.len(), 1);
    assert_eq!(targets[0].workspace_path, record.workspace_path);
    assert_eq!(
        shared_store
            .claim_workspace_cleanup_hook(
                "legacy-runtime-workflow",
                &record.workspace_path,
                WorkspaceCleanupHook::Workflow,
            )
            .await?,
        Some(false),
        "legacy backfill must preserve the workflow hook claim"
    );
    assert_eq!(
        shared_store
            .claim_workspace_cleanup_hook(
                "legacy-runtime-workflow",
                &record.workspace_path,
                WorkspaceCleanupHook::Manager,
            )
            .await?,
        Some(false),
        "legacy backfill must preserve the manager hook claim"
    );

    shared_schema.cleanup_with_pool(&setup_pool).await?;
    setup_pool.close().await;
    Ok(())
}

#[tokio::test]
async fn repository_lease_modes_are_global_across_postgres_schemas() -> anyhow::Result<()> {
    let database_url = match harness_core::db::resolve_test_database_url(None) {
        Ok(url) => url,
        Err(_) => return Ok(()),
    };
    let dir = tempfile::tempdir().expect("tempdir");
    let setup_pool = harness_core::db::pg_open_pool(&database_url).await?;
    let mut schema_a = TestSchemaGuard::new(&database_url, "workspace_lease_scope_test")?;
    let mut schema_b = TestSchemaGuard::new(&database_url, "workspace_lease_scope_test")?;
    let context_a =
        harness_core::db::PgStoreContext::from_schema(schema_a.schema(), Some(&database_url))?;
    let context_b =
        harness_core::db::PgStoreContext::from_schema(schema_b.schema(), Some(&database_url))?;
    let data_a = dir.path().join("instance-a");
    let data_b = dir.path().join("instance-b");
    std::fs::create_dir_all(&data_a)?;
    std::fs::create_dir_all(&data_b)?;
    crate::task_db::TaskDb::open_shared_with_data_dir(&context_a, &setup_pool, &data_a).await?;
    crate::task_db::TaskDb::open_shared_with_data_dir(&context_b, &setup_pool, &data_b).await?;
    let store_a =
        WorkspaceLeaseStore::open_shared_with_data_dir(&context_a, &setup_pool, &data_a).await?;
    let store_b =
        WorkspaceLeaseStore::open_shared_with_data_dir(&context_b, &setup_pool, &data_b).await?;

    let lease_a = store_a
        .try_acquire_repository_write_lease("/repo/owner/project")
        .await?
        .expect("first Harness instance should acquire the repository write lease");
    assert!(
        store_b
            .try_acquire_repository_write_lease("/repo/owner/project")
            .await?
            .is_none(),
        "a second Harness instance in another schema must not acquire the same repository"
    );
    drop(lease_a);
    let lease_b = store_b
        .try_acquire_repository_write_lease("/repo/owner/project")
        .await?
        .expect("lease should be released when the owning connection is dropped");
    drop(lease_b);

    let shared_a = store_a
        .try_acquire_repository_shared_lease("/repo/owner/project")
        .await?
        .expect("the first multi-writer should acquire a shared repository lease");
    let shared_b = store_b
        .try_acquire_repository_shared_lease("/repo/owner/project")
        .await?
        .expect("a second multi-writer should share the repository lease");
    assert!(
        store_b
            .try_acquire_repository_write_lease("/repo/owner/project")
            .await?
            .is_none(),
        "an N-to-1 transition must wait for every shared writer"
    );
    drop(shared_a);
    assert!(
        store_a
            .try_acquire_repository_write_lease("/repo/owner/project")
            .await?
            .is_none(),
        "an exclusive writer must keep waiting while one shared writer remains"
    );
    drop(shared_b);
    let exclusive = store_a
        .try_acquire_repository_write_lease("/repo/owner/project")
        .await?
        .expect("the single-writer transition should acquire after shared writers drain");
    assert!(
        store_b
            .try_acquire_repository_shared_lease("/repo/owner/project")
            .await?
            .is_none(),
        "a 1-to-N transition must wait for the exclusive writer"
    );
    drop(exclusive);
    let shared_after_transition = store_b
        .try_acquire_repository_shared_lease("/repo/owner/project")
        .await?
        .expect("shared writers should resume after the exclusive writer drains");
    drop(shared_after_transition);

    schema_a.cleanup_with_pool(&setup_pool).await?;
    schema_b.cleanup_with_pool(&setup_pool).await?;
    setup_pool.close().await;
    Ok(())
}

#[tokio::test]
async fn immediate_repository_lease_reopens_connections_after_drop() -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }
    let dir = tempfile::tempdir()?;
    let store = WorkspaceLeaseStore::open_with_repository_lock_capacity(
        &dir.path().join("immediate-repository-lease"),
        1,
    )
    .await?;

    for _ in 0..3 {
        let lease = store
            .try_acquire_repository_write_lease_now("/repo/repeated-immediate")
            .await?
            .expect("an idle repository should acquire immediately");
        drop(lease);
    }
    Ok(())
}

#[tokio::test]
async fn repository_lease_reports_terminated_lock_session() -> anyhow::Result<()> {
    let database_url = match harness_core::db::resolve_test_database_url(None) {
        Ok(url) => url,
        Err(_) => return Ok(()),
    };
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(3)
        .connect(&database_url)
        .await?;
    let store = WorkspaceLeaseStore::for_repository_lock_pool_test(pool);
    let lease = store
        .try_acquire_repository_write_lease("/repo/terminated-lock-session")
        .await?
        .expect("repository lock should acquire");
    let mut lost = lease.loss_receiver();

    assert!(store.terminate_repository_lease_for_test(&lease).await?);
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while *lost.borrow() != RepositoryLeaseState::Lost {
            lost.changed()
                .await
                .expect("repository lock monitor should remain available");
        }
    })
    .await
    .map_err(|_| anyhow::anyhow!("repository lock loss was not reported"))?;
    let replacement = store
        .try_acquire_repository_write_lease("/repo/terminated-lock-session")
        .await?
        .expect("a lost lease must close its session and release its lock slot");
    drop(replacement);
    Ok(())
}

#[tokio::test]
async fn queued_exclusive_repository_lease_blocks_later_shared_writer() -> anyhow::Result<()> {
    let database_url = match harness_core::db::resolve_test_database_url(None) {
        Ok(url) => url,
        Err(_) => return Ok(()),
    };
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(4)
        .connect(&database_url)
        .await?;
    let store = WorkspaceLeaseStore::for_repository_lock_pool_test(pool);
    let project_key = "/repo/queued-exclusive";
    let initial_shared = store
        .acquire_queued_repository_shared_lease(project_key)
        .await?;

    let exclusive_store = store.clone();
    let exclusive = tokio::spawn(async move {
        exclusive_store
            .acquire_queued_repository_write_lease(project_key)
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if store
                .queued_repository_lock_waiter_count_for_test(project_key)
                .await?
                > 0
            {
                return Ok::<(), anyhow::Error>(());
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .map_err(|_| anyhow::anyhow!("exclusive lease did not enter PostgreSQL's wait queue"))??;

    let later_shared_store = store.clone();
    let later_shared = tokio::spawn(async move {
        later_shared_store
            .acquire_queued_repository_shared_lease(project_key)
            .await
    });
    drop(initial_shared);
    let exclusive_lease = tokio::time::timeout(std::time::Duration::from_secs(5), exclusive)
        .await
        .map_err(|_| anyhow::anyhow!("queued exclusive lease starved"))???;
    assert!(
        !later_shared.is_finished(),
        "a later shared writer must not bypass a queued exclusive lease"
    );
    drop(exclusive_lease);
    let shared_lease = tokio::time::timeout(std::time::Duration::from_secs(5), later_shared)
        .await
        .map_err(|_| anyhow::anyhow!("later shared lease did not resume"))???;
    drop(shared_lease);
    Ok(())
}

#[tokio::test]
async fn live_capacity_downshift_rejects_old_shared_waiter_and_unblocks_exclusive(
) -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    async fn hold_workspace_admission(
        manager: &WorkspaceManager,
        source_repo: &std::path::Path,
        task_id: &str,
    ) -> anyhow::Result<(
        Option<RepositoryWriteLease>,
        tokio::sync::OwnedSemaphorePermit,
    )> {
        let (lease, capacity) = manager
            .acquire_repository_lease_from_current_config(source_repo)
            .await?;
        let admission = manager
            .acquire_resolved_workspace_admission(
                &TaskId::from_str(task_id),
                source_repo,
                Some("owner/repo"),
                1,
                lease,
                capacity,
            )
            .await
            .map_err(|error| anyhow::anyhow!(error.to_string()))?;
        match admission {
            super::workspace_repository::ResolvedWorkspaceAdmission::Acquired {
                repository_write_lease,
                pool_permit,
                slot_guard,
                ..
            } => {
                drop(slot_guard);
                Ok((repository_write_lease, pool_permit))
            }
            super::workspace_repository::ResolvedWorkspaceAdmission::Reused(_) => {
                anyhow::bail!("test admission unexpectedly reused an active workspace")
            }
        }
    }

    let source = tempfile::tempdir()?;
    let workspaces = tempfile::tempdir()?;
    let data_dir = tempfile::tempdir()?;
    let registry_dir = tempfile::tempdir()?;
    let lease_dir = tempfile::tempdir()?;
    let source_repo = source.path().canonicalize()?;
    let registry = crate::project_registry::ProjectRegistry::open(
        &registry_dir.path().join("live-capacity-registry"),
    )
    .await?;
    registry
        .register(crate::project_registry::Project {
            id: "live-capacity-project".to_string(),
            root: source_repo.clone(),
            name: None,
            max_concurrent: Some(2),
            default_agent: None,
            active: true,
            created_at: chrono::Utc::now().to_rfc3339(),
        })
        .await?;
    let mut config = harness_core::config::HarnessConfig::default();
    config.server.project_root = source_repo.clone();
    config.server.data_dir = data_dir.path().to_path_buf();
    let server = std::sync::Arc::new(crate::server::HarnessServer::new(
        config,
        crate::thread_manager::ThreadManager::new(),
        harness_agents::registry::AgentRegistry::new("test"),
    ));
    let store = std::sync::Arc::new(
        WorkspaceLeaseStore::open(&lease_dir.path().join("live-capacity-leases")).await?,
    );
    let manager = std::sync::Arc::new(WorkspaceManager::new_with_pool_and_capacity_source(
        WorkspaceConfig {
            root: workspaces.path().to_path_buf(),
            ..Default::default()
        },
        WorkspacePoolConfig::new(2, std::collections::HashMap::new()),
        Some(store),
        server,
        Some(registry.clone()),
    )?);

    let held_a = hold_workspace_admission(&manager, &source_repo, "capacity-held-a").await?;
    let held_b = hold_workspace_admission(&manager, &source_repo, "capacity-held-b").await?;
    let (old_shared_lease, old_capacity) = manager
        .acquire_repository_lease_from_current_config(&source_repo)
        .await?;
    assert_eq!(old_capacity, 2);
    let waiter_manager = manager.clone();
    let waiter_source = source_repo.clone();
    let old_waiter = tokio::spawn(async move {
        waiter_manager
            .acquire_resolved_workspace_admission(
                &TaskId::from_str("capacity-old-waiter"),
                &waiter_source,
                Some("owner/repo"),
                1,
                old_shared_lease,
                old_capacity,
            )
            .await
    });

    registry
        .register(crate::project_registry::Project {
            id: "live-capacity-project".to_string(),
            root: source_repo.clone(),
            name: None,
            max_concurrent: Some(1),
            default_agent: None,
            active: true,
            created_at: chrono::Utc::now().to_rfc3339(),
        })
        .await?;
    let exclusive_manager = manager.clone();
    let exclusive_source = source_repo.clone();
    let exclusive = tokio::spawn(async move {
        exclusive_manager
            .acquire_repository_write_lease(&exclusive_source)
            .await
    });

    drop(held_a);
    let stale_result = tokio::time::timeout(std::time::Duration::from_secs(5), old_waiter)
        .await
        .map_err(|_| anyhow::anyhow!("old shared waiter did not revalidate capacity"))??;
    let stale_error = match stale_result {
        Err(error) => error,
        Ok(_) => anyhow::bail!("old shared waiter started after capacity dropped to one"),
    };
    assert!(stale_error
        .to_string()
        .contains("live workspace capacity changed"));
    assert!(
        !exclusive.is_finished(),
        "exclusive admission must still wait for the final old shared holder"
    );
    drop(held_b);
    let exclusive = tokio::time::timeout(std::time::Duration::from_secs(5), exclusive)
        .await
        .map_err(|_| {
            anyhow::anyhow!("exclusive admission did not converge after shared drain")
        })???;
    drop(exclusive);
    Ok(())
}

#[tokio::test]
async fn repository_write_lease_waits_when_dedicated_pool_is_full() -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }
    let dir = tempfile::tempdir()?;
    let store = WorkspaceLeaseStore::open(&dir.path().join("repository-lock-pool")).await?;
    let pool_capacity = store.repository_lock_pool_capacity();
    let mut held_leases = Vec::with_capacity(pool_capacity as usize);
    for index in 0..pool_capacity {
        let lease = store
            .try_acquire_repository_write_lease(&format!("/repo/owner/held-{index}"))
            .await?
            .ok_or_else(|| anyhow::anyhow!("repository lease {index} should acquire"))?;
        held_leases.push(lease);
    }
    let second = store.try_acquire_repository_write_lease_for_test(
        "/repo/owner/second",
        std::time::Duration::from_millis(100),
    );
    tokio::pin!(second);

    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(250), &mut second)
            .await
            .is_err(),
        "pool exhaustion must keep waiting instead of returning a timeout error"
    );
    held_leases.pop();
    let second = tokio::time::timeout(std::time::Duration::from_secs(2), &mut second)
        .await
        .map_err(|_| anyhow::anyhow!("waiting repository lease did not resume"))??
        .ok_or_else(|| anyhow::anyhow!("unrelated repository lease should acquire"))?;
    drop(second);
    drop(held_leases);
    Ok(())
}

#[tokio::test]
async fn persisted_slot_wait_releases_repository_lease_for_cleanup() -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }
    let source = tempfile::tempdir()?;
    init_git_repo(source.path());
    let branch = current_branch(source.path());
    let workspaces = tempfile::tempdir()?;
    let lease_db = tempfile::tempdir()?;
    let store = std::sync::Arc::new(
        WorkspaceLeaseStore::open(&lease_db.path().join("slot-readmission")).await?,
    );
    let manager = std::sync::Arc::new(WorkspaceManager::new_with_pool(
        WorkspaceConfig {
            root: workspaces.path().to_path_buf(),
            ..Default::default()
        },
        WorkspacePoolConfig::new(1, std::collections::HashMap::new()),
        Some(store.clone()),
    )?);
    let occupied = WorkspaceLeaseRecord {
        project_key: crate::workspace_pool::project_limit_key(source.path()),
        slot_index: 0,
        task_id: TaskId::from_str("occupied-slot"),
        workspace_key: "occupied-slot".to_string(),
        workspace_path: workspaces.path().join("occupied-slot"),
        source_repo: source.path().to_path_buf(),
        repo: Some("owner/repo".to_string()),
        runtime_workflow_id: Some("occupied-workflow".to_string()),
        owner_session: "occupied-session".to_string(),
        run_generation: 1,
        acquisition_id: Some("occupied-acquisition".to_string()),
        process_id: std::process::id(),
        process_started_at: WorkspaceLeaseStore::current_process_started_at()?,
    };
    assert!(store.try_acquire_lease(&occupied).await?);

    let initial_repository_lease = manager
        .acquire_repository_write_lease(source.path())
        .await?;
    let creator_manager = manager.clone();
    let creator_source = source.path().to_path_buf();
    let creator_branch = branch.clone();
    let creator = tokio::spawn(async move {
        creator_manager
            .create_workspace_with_options(
                &TaskId::from_str("replacement-slot"),
                &creator_source,
                "origin",
                &creator_branch,
                1,
                Some("issue:readmission"),
                Some("owner/repo"),
                WorkspaceCreateOptions {
                    require_remote_head: false,
                    ..Default::default()
                },
            )
            .await
    });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let cleanup_manager = manager.clone();
    let cleanup_store = store.clone();
    let cleanup_source = source.path().to_path_buf();
    let cleanup_record = occupied.clone();
    let cleanup = tokio::spawn(async move {
        let repository_lease = cleanup_manager
            .acquire_repository_write_lease_for_cleanup(&cleanup_source)
            .await?;
        cleanup_store
            .complete_owned_workspace(
                &cleanup_record.project_key,
                cleanup_record.slot_index,
                &cleanup_record.task_id,
                &cleanup_record.owner_session,
                cleanup_record.run_generation,
                cleanup_record
                    .acquisition_id
                    .as_deref()
                    .expect("occupied acquisition"),
            )
            .await?;
        drop(repository_lease);
        Ok::<(), anyhow::Error>(())
    });
    drop(initial_repository_lease);

    tokio::time::timeout(std::time::Duration::from_secs(5), cleanup)
        .await
        .map_err(|_| anyhow::anyhow!("cleanup starved behind the persisted-slot waiter"))???;
    let workspace = tokio::time::timeout(std::time::Duration::from_secs(5), creator)
        .await
        .map_err(|_| anyhow::anyhow!("workspace creation did not resume after cleanup"))???;
    manager
        .remove_workspace_acquisition(
            &TaskId::from_str("replacement-slot"),
            &workspace.acquisition_id,
        )
        .await?;
    Ok(())
}

#[tokio::test]
async fn workspace_creation_accepts_preacquired_repository_write_lease() -> anyhow::Result<()> {
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
    let mgr = WorkspaceManager::new_with_pool(
        WorkspaceConfig {
            root: workspaces.path().to_path_buf(),
            ..Default::default()
        },
        WorkspacePoolConfig::new(2, std::collections::HashMap::new()),
        Some(store.clone()),
    )?;
    let task_id = harness_core::types::TaskId("preacquired-repository-lease".to_string());
    let repository_write_lease = mgr.acquire_repository_write_lease(source.path()).await?;

    let workspace = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        mgr.create_workspace_with_options(
            &task_id,
            source.path(),
            "origin",
            &branch,
            1,
            Some("issue:1302"),
            Some("owner/repo"),
            WorkspaceCreateOptions {
                require_remote_head: false,
                repository_write_lease: RepositoryWriteLeaseInput::Held(repository_write_lease),
                ..Default::default()
            },
        ),
    )
    .await
    .map_err(|_| {
        anyhow::anyhow!("workspace creation tried to acquire the repository lease twice")
    })??;
    assert!(workspace.workspace_path.exists());
    mgr.remove_workspace(&task_id).await?;

    let reused_task_id = harness_core::types::TaskId("attach-preacquired-lease".to_string());
    mgr.create_workspace_with_options(
        &reused_task_id,
        source.path(),
        "origin",
        &branch,
        1,
        Some("issue:1303"),
        Some("owner/repo"),
        WorkspaceCreateOptions {
            require_remote_head: false,
            repository_write_lease: RepositoryWriteLeaseInput::NotRequired,
            ..Default::default()
        },
    )
    .await?;
    assert!(
        mgr.active
            .get(&reused_task_id)
            .is_some_and(|active| active._repository_write_lease.is_none()),
        "the multi-writer workspace should start without a repository lease"
    );
    let repository_write_lease = mgr.acquire_repository_write_lease(source.path()).await?;
    mgr.create_workspace_with_options(
        &reused_task_id,
        source.path(),
        "origin",
        &branch,
        1,
        Some("issue:1303"),
        Some("owner/repo"),
        WorkspaceCreateOptions {
            require_remote_head: false,
            repository_write_lease: RepositoryWriteLeaseInput::Held(repository_write_lease),
            ..Default::default()
        },
    )
    .await?;
    assert!(
        mgr.active
            .get(&reused_task_id)
            .is_some_and(|active| active._repository_write_lease.is_some()),
        "a live capacity change to single-writer must attach the preacquired lease to a reused workspace"
    );
    let mut lost = mgr
        .repository_lease_lost_for_task(&reused_task_id)
        .expect("the active repository lease should expose liveness");
    {
        let active = mgr
            .active
            .get(&reused_task_id)
            .expect("the reused workspace should remain active");
        store
            .terminate_repository_lease_for_test(
                active
                    ._repository_write_lease
                    .as_ref()
                    .expect("the active workspace should retain its repository lease"),
            )
            .await?;
    }
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while *lost.borrow() != RepositoryLeaseState::Lost {
            lost.changed().await?;
        }
        Ok::<(), anyhow::Error>(())
    })
    .await
    .map_err(|_| anyhow::anyhow!("terminated repository lease was not reported"))??;
    mgr.create_workspace_with_options(
        &reused_task_id,
        source.path(),
        "origin",
        &branch,
        1,
        Some("issue:1303"),
        Some("owner/repo"),
        WorkspaceCreateOptions {
            require_remote_head: false,
            ..Default::default()
        },
    )
    .await?;
    assert!(
        mgr.repository_lease_lost_for_task(&reused_task_id)
            .is_some_and(|receiver| *receiver.borrow() == RepositoryLeaseState::Healthy),
        "default same-workspace retry should resolve current capacity and replace a dead repository lease"
    );
    let project_key = crate::workspace_pool::project_limit_key(source.path());
    assert!(
        store
            .try_acquire_repository_write_lease_now(&project_key)
            .await?
            .is_none(),
        "the reused workspace must retain the attached repository lease"
    );
    mgr.remove_workspace(&reused_task_id).await?;
    Ok(())
}

#[tokio::test]
async fn cross_manager_reconciliation_waits_for_repository_writer() -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }
    let source = tempfile::tempdir()?;
    init_git_repo(source.path());
    let branch = current_branch(source.path());
    let workspaces = tempfile::tempdir()?;
    let lease_db = tempfile::tempdir()?;
    let store = std::sync::Arc::new(
        WorkspaceLeaseStore::open(&lease_db.path().join("reconcile-race")).await?,
    );
    let config = WorkspaceConfig {
        root: workspaces.path().to_path_buf(),
        ..Default::default()
    };
    let pool_config = WorkspacePoolConfig::new(2, std::collections::HashMap::new());
    let manager_a = std::sync::Arc::new(WorkspaceManager::new_with_pool(
        config.clone(),
        pool_config.clone(),
        Some(store.clone()),
    )?);
    let manager_b = std::sync::Arc::new(WorkspaceManager::new_with_pool(
        config,
        pool_config,
        Some(store),
    )?);
    let task_id = harness_core::types::TaskId("reconcile-race-writer".to_string());
    let workspace = manager_a
        .create_workspace(
            &task_id,
            source.path(),
            "origin",
            &branch,
            1,
            Some("issue:1304"),
            Some("owner/repo"),
        )
        .await?;

    let manager_b_for_cleanup = manager_b.clone();
    let source_path = source.path().to_path_buf();
    let workspace_path = workspace.workspace_path.clone();
    let cleanup = tokio::spawn(async move {
        manager_b_for_cleanup
            .cleanup_reconciliation_workspace_path(&source_path, &workspace_path)
            .await
    });
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert!(
        !cleanup.is_finished(),
        "reconciliation must wait while another manager holds a shared writer lease"
    );
    assert!(workspace.workspace_path.exists());

    manager_a.release_workspace(&task_id).await;
    let removed = tokio::time::timeout(std::time::Duration::from_secs(5), cleanup)
        .await
        .map_err(|_| anyhow::anyhow!("reconciliation did not resume after writer release"))???;
    assert!(removed);
    assert!(!workspace.workspace_path.exists());
    Ok(())
}

#[tokio::test]
async fn repository_write_lease_propagates_sqlx_pool_timeout() -> anyhow::Result<()> {
    let unavailable_pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .acquire_timeout(std::time::Duration::from_millis(50))
        .connect_lazy("postgres://postgres@127.0.0.1:1/harness_test")?;
    let store = WorkspaceLeaseStore::for_repository_lock_pool_test(unavailable_pool);

    let result = store
        .try_acquire_repository_write_lease_for_test(
            "/repo/owner/unavailable",
            std::time::Duration::from_millis(200),
        )
        .await;
    let error = match result {
        Err(error) => error,
        Ok(_) => {
            anyhow::bail!("database connectivity timeout must not be retried as pool saturation")
        }
    };
    assert!(
        error.to_string().contains("timed out acquiring"),
        "unexpected database timeout error: {error}"
    );
    Ok(())
}

#[tokio::test]
async fn repository_write_lease_propagates_outer_acquire_timeout() -> anyhow::Result<()> {
    let unavailable_pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .acquire_timeout(std::time::Duration::from_secs(1))
        .connect_lazy("postgres://postgres@127.0.0.1:1/harness_test")?;
    let store = WorkspaceLeaseStore::for_repository_lock_pool_test(unavailable_pool);

    let result = store
        .try_acquire_repository_write_lease_for_test(
            "/repo/owner/unavailable",
            std::time::Duration::from_millis(50),
        )
        .await;
    let error = match result {
        Err(error) => error,
        Ok(_) => anyhow::bail!("outer connectivity timeout must not be retried as pool saturation"),
    };
    assert!(
        error.to_string().contains("timed out acquiring"),
        "unexpected outer database timeout error: {error}"
    );
    Ok(())
}

#[tokio::test]
async fn workspace_lease_store_does_not_steal_live_foreign_slot() -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let store = WorkspaceLeaseStore::open(&dir.path().join("workspace-leases")).await?;
    let first_task = harness_core::types::TaskId("lease-store-first".to_string());
    let second_task = harness_core::types::TaskId("lease-store-second".to_string());
    let process_started_at = WorkspaceLeaseStore::current_process_started_at()?;
    let first_record = WorkspaceLeaseRecord {
        project_key: "project-a".to_string(),
        slot_index: 0,
        task_id: first_task,
        workspace_key: "workspace-a".to_string(),
        workspace_path: dir.path().join("workspaces/project-a-slot-0"),
        source_repo: dir.path().join("repo"),
        repo: Some("owner/repo".to_string()),
        runtime_workflow_id: Some("workflow-1".to_string()),
        owner_session: "session-a".to_string(),
        run_generation: 1,
        acquisition_id: Some("acquisition-first".to_string()),
        process_id: std::process::id(),
        process_started_at,
    };
    let second_record = WorkspaceLeaseRecord {
        task_id: second_task,
        owner_session: "session-b".to_string(),
        runtime_workflow_id: Some("workflow-2".to_string()),
        ..first_record.clone()
    };

    assert!(store.try_acquire_lease(&first_record).await?);
    assert!(
        !store.try_acquire_lease(&second_record).await?,
        "foreign live lease must not be overwritten"
    );
    let leased = store.list_leased().await?;
    assert_eq!(leased.len(), 1);
    assert_eq!(leased[0].task_id, first_record.task_id);
    assert_eq!(leased[0].owner_session, first_record.owner_session);

    assert!(
        store
            .release_owned_slot(
                &first_record.project_key,
                first_record.slot_index,
                &first_record.task_id,
                &first_record.owner_session,
                first_record.run_generation,
                first_record
                    .acquisition_id
                    .as_deref()
                    .expect("acquisition ID"),
            )
            .await?
    );
    assert!(
        store.try_acquire_lease(&second_record).await?,
        "released slots should be reusable"
    );

    Ok(())
}

#[tokio::test]
async fn workspace_lease_store_releases_only_dead_foreign_processes() -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let store = WorkspaceLeaseStore::open(&dir.path().join("workspace-leases")).await?;
    let process_started_at = WorkspaceLeaseStore::current_process_started_at()?;
    let live_record = WorkspaceLeaseRecord {
        project_key: "project-a".to_string(),
        slot_index: 0,
        task_id: harness_core::types::TaskId("live-foreign-task".to_string()),
        workspace_key: "workspace-live".to_string(),
        workspace_path: dir.path().join("workspaces/project-a-slot-0"),
        source_repo: dir.path().join("repo"),
        repo: Some("owner/repo".to_string()),
        runtime_workflow_id: Some("workflow-live".to_string()),
        owner_session: "session-live".to_string(),
        run_generation: 1,
        acquisition_id: Some("acquisition-live".to_string()),
        process_id: std::process::id(),
        process_started_at,
    };
    let dead_record = WorkspaceLeaseRecord {
        slot_index: 1,
        task_id: harness_core::types::TaskId("dead-foreign-task".to_string()),
        workspace_key: "workspace-dead".to_string(),
        workspace_path: dir.path().join("workspaces/project-a-slot-1"),
        runtime_workflow_id: Some("workflow-dead".to_string()),
        owner_session: "session-dead".to_string(),
        process_id: u32::MAX,
        process_started_at: 1,
        ..live_record.clone()
    };

    assert!(store.try_acquire_lease(&live_record).await?);
    assert!(store.try_acquire_lease(&dead_record).await?);

    let released = store
        .release_foreign_orphaned_leases("current-session")
        .await?;
    assert_eq!(released, 1);
    let leased = store.list_leased().await?;
    assert_eq!(leased.len(), 1);
    assert_eq!(leased[0].task_id, live_record.task_id);

    Ok(())
}

#[tokio::test]
async fn workspace_lease_store_releases_pid_reuse_mismatch() -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let store = WorkspaceLeaseStore::open(&dir.path().join("workspace-leases")).await?;
    let current_started_at = WorkspaceLeaseStore::current_process_started_at()?;
    let stale_started_at = current_started_at.saturating_add(1);
    let record = WorkspaceLeaseRecord {
        project_key: "project-a".to_string(),
        slot_index: 0,
        task_id: harness_core::types::TaskId("pid-reuse-task".to_string()),
        workspace_key: "workspace-reused-pid".to_string(),
        workspace_path: dir.path().join("workspaces/project-a-slot-0"),
        source_repo: dir.path().join("repo"),
        repo: Some("owner/repo".to_string()),
        runtime_workflow_id: Some("workflow-reused-pid".to_string()),
        owner_session: "session-stale".to_string(),
        run_generation: 1,
        acquisition_id: Some("acquisition-stale".to_string()),
        process_id: std::process::id(),
        process_started_at: stale_started_at,
    };

    assert!(store.try_acquire_lease(&record).await?);
    let released = store
        .release_foreign_orphaned_leases("current-session")
        .await?;

    assert_eq!(released, 1);
    assert!(
        store.list_leased().await?.is_empty(),
        "same pid with different start time should be treated as stale"
    );

    Ok(())
}

#[tokio::test]
async fn shared_lease_store_allocates_next_slot_without_stealing_live_foreign_lease(
) -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }
    let source = tempfile::tempdir().expect("tempdir");
    init_git_repo(source.path());
    let branch = current_branch(source.path());

    let workspaces = tempfile::tempdir().expect("tempdir");
    let lease_db = tempfile::tempdir().expect("tempdir");
    let store = std::sync::Arc::new(
        WorkspaceLeaseStore::open(&lease_db.path().join("workspace-leases")).await?,
    );
    let config = WorkspaceConfig {
        root: workspaces.path().to_path_buf(),
        ..Default::default()
    };
    let pool_config = WorkspacePoolConfig::new(2, std::collections::HashMap::new());
    let mgr_a =
        WorkspaceManager::new_with_pool(config.clone(), pool_config.clone(), Some(store.clone()))?;
    let mgr_b = WorkspaceManager::new_with_pool(config, pool_config, Some(store.clone()))?;
    let first_task = harness_core::types::TaskId("shared-store-first".to_string());
    let second_task = harness_core::types::TaskId("shared-store-second".to_string());

    let first = mgr_a
        .create_workspace(
            &first_task,
            source.path(),
            "origin",
            &branch,
            1,
            Some("issue:42"),
            Some("owner/repo"),
        )
        .await?;
    let second = mgr_b
        .create_workspace(
            &second_task,
            source.path(),
            "origin",
            &branch,
            1,
            Some("issue:43"),
            Some("owner/repo"),
        )
        .await?;

    assert_eq!(first.slot_index, 0);
    assert_eq!(second.slot_index, 1);
    assert_ne!(first.workspace_path, second.workspace_path);
    assert_eq!(store.list_leased().await?.len(), 2);

    mgr_a.remove_workspace(&first_task).await?;
    mgr_b.remove_workspace(&second_task).await?;

    Ok(())
}

#[tokio::test]
async fn shared_lease_store_waits_when_persisted_slots_are_full() -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }
    let source = tempfile::tempdir().expect("tempdir");
    init_git_repo(source.path());
    let branch = current_branch(source.path());

    let workspaces = tempfile::tempdir().expect("tempdir");
    let lease_db = tempfile::tempdir().expect("tempdir");
    let store = std::sync::Arc::new(
        WorkspaceLeaseStore::open(&lease_db.path().join("workspace-leases")).await?,
    );
    let config = WorkspaceConfig {
        root: workspaces.path().to_path_buf(),
        ..Default::default()
    };
    let pool_config = WorkspacePoolConfig::new(1, std::collections::HashMap::new());
    let mgr_a =
        WorkspaceManager::new_with_pool(config.clone(), pool_config.clone(), Some(store.clone()))?;
    let mgr_b = std::sync::Arc::new(WorkspaceManager::new_with_pool(
        config,
        pool_config,
        Some(store.clone()),
    )?);
    let first_task = harness_core::types::TaskId("shared-store-full-first".to_string());
    let second_task = harness_core::types::TaskId("shared-store-full-second".to_string());

    let first = mgr_a
        .create_workspace(
            &first_task,
            source.path(),
            "origin",
            &branch,
            1,
            Some("issue:42"),
            Some("owner/repo"),
        )
        .await?;
    assert_eq!(first.slot_index, 0);

    let source_path = source.path().to_path_buf();
    let branch_for_second = branch.clone();
    let second_task_for_spawn = second_task.clone();
    let mgr_b_for_spawn = mgr_b.clone();
    let second_handle = tokio::spawn(async move {
        mgr_b_for_spawn
            .create_workspace(
                &second_task_for_spawn,
                &source_path,
                "origin",
                &branch_for_second,
                1,
                Some("issue:43"),
                Some("owner/repo"),
            )
            .await
    });

    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert!(
        !second_handle.is_finished(),
        "second manager should wait while the shared lease table is full"
    );

    mgr_a.release_workspace(&first_task).await;
    let second = tokio::time::timeout(std::time::Duration::from_secs(5), second_handle)
        .await
        .expect("second acquire should unblock")
        .expect("second task should join")?;
    assert_eq!(second.slot_index, 0);
    assert_eq!(second.workspace_path, first.workspace_path);

    mgr_b.remove_workspace(&second_task).await?;

    Ok(())
}

#[tokio::test]
async fn shared_lease_store_enforces_project_capacity_across_repo_slugs() -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }
    let source = tempfile::tempdir().expect("tempdir");
    init_git_repo(source.path());
    let branch = current_branch(source.path());

    let workspaces = tempfile::tempdir().expect("tempdir");
    let lease_db = tempfile::tempdir().expect("tempdir");
    let store = std::sync::Arc::new(
        WorkspaceLeaseStore::open(&lease_db.path().join("workspace-leases")).await?,
    );
    let config = WorkspaceConfig {
        root: workspaces.path().to_path_buf(),
        ..Default::default()
    };
    let pool_config = WorkspacePoolConfig::new(1, std::collections::HashMap::new());
    let mgr_a =
        WorkspaceManager::new_with_pool(config.clone(), pool_config.clone(), Some(store.clone()))?;
    let mgr_b = std::sync::Arc::new(WorkspaceManager::new_with_pool(
        config,
        pool_config,
        Some(store.clone()),
    )?);
    let first_task = harness_core::types::TaskId("shared-cross-repo-first".to_string());
    let second_task = harness_core::types::TaskId("shared-cross-repo-second".to_string());

    let first = mgr_a
        .create_workspace(
            &first_task,
            source.path(),
            "origin",
            &branch,
            1,
            Some("issue:42"),
            Some("owner/repo-a"),
        )
        .await?;
    assert_eq!(first.slot_index, 0);

    let source_path = source.path().to_path_buf();
    let branch_for_second = branch.clone();
    let second_task_for_spawn = second_task.clone();
    let mgr_b_for_spawn = mgr_b.clone();
    let second_handle = tokio::spawn(async move {
        mgr_b_for_spawn
            .create_workspace(
                &second_task_for_spawn,
                &source_path,
                "origin",
                &branch_for_second,
                1,
                Some("issue:43"),
                Some("owner/repo-b"),
            )
            .await
    });

    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert!(
        !second_handle.is_finished(),
        "second manager should wait for the shared source-project persisted slot even with a different repo slug"
    );

    mgr_a.release_workspace(&first_task).await;
    let second = tokio::time::timeout(std::time::Duration::from_secs(5), second_handle)
        .await
        .expect("second acquire should unblock")
        .expect("second task should join")?;
    assert_eq!(second.slot_index, 0);
    assert_eq!(
        first.project_key, second.project_key,
        "persisted lease capacity key should be source-project scoped"
    );
    assert_ne!(
        first.workspace_path, second.workspace_path,
        "repo slug remains part of the workspace slot path"
    );

    mgr_b.remove_workspace(&second_task).await?;

    Ok(())
}

#[tokio::test]
async fn remove_workspace_releases_persisted_slot_after_cleanup_hook_finishes() -> anyhow::Result<()>
{
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }
    let source = tempfile::tempdir().expect("tempdir");
    init_git_repo(source.path());
    let branch = current_branch(source.path());

    let workspaces = tempfile::tempdir().expect("tempdir");
    let lease_db = tempfile::tempdir().expect("tempdir");
    let store = std::sync::Arc::new(
        WorkspaceLeaseStore::open(&lease_db.path().join("workspace-leases")).await?,
    );
    let config = WorkspaceConfig {
        root: workspaces.path().to_path_buf(),
        before_remove_hook: Some("sh hold-remove.sh".to_string()),
        hook_timeout_secs: 5,
        ..Default::default()
    };
    let mgr = std::sync::Arc::new(WorkspaceManager::new_with_pool(
        config,
        WorkspacePoolConfig::new(1, std::collections::HashMap::new()),
        Some(store.clone()),
    )?);
    let task_id = harness_core::types::TaskId("remove-holds-lease".to_string());

    let lease = mgr
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
    std::fs::write(lease.workspace_path.join("hold-remove.sh"), "sleep 1\n")?;
    assert_eq!(store.list_leased().await?.len(), 1);

    let task_id_for_spawn = task_id.clone();
    let mgr_for_spawn = mgr.clone();
    let remove_handle =
        tokio::spawn(async move { mgr_for_spawn.remove_workspace(&task_id_for_spawn).await });

    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert_eq!(
        store.list_leased().await?.len(),
        1,
        "persisted lease should remain held while removal hook is still running"
    );

    tokio::time::timeout(std::time::Duration::from_secs(5), remove_handle)
        .await
        .expect("remove should finish")
        .expect("remove task should join")?;
    assert!(
        store.list_leased().await?.is_empty(),
        "persisted lease should release after cleanup completes"
    );

    Ok(())
}

#[tokio::test]
async fn cleanup_workspace_for_retry_releases_persisted_slot_after_cleanup_finishes(
) -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }
    let source = tempfile::tempdir().expect("tempdir");
    init_git_repo(source.path());
    let branch = current_branch(source.path());

    let workspaces = tempfile::tempdir().expect("tempdir");
    let lease_db = tempfile::tempdir().expect("tempdir");
    let store = std::sync::Arc::new(
        WorkspaceLeaseStore::open(&lease_db.path().join("workspace-leases")).await?,
    );
    let config = WorkspaceConfig {
        root: workspaces.path().to_path_buf(),
        ..Default::default()
    };
    let mgr = std::sync::Arc::new(WorkspaceManager::new_with_pool(
        config,
        crate::workspace_pool::WorkspacePoolConfig::new(1, std::collections::HashMap::new()),
        Some(store.clone()),
    )?);
    let task_id = harness_core::types::TaskId("retry-cleanup-holds-lease".to_string());

    let lease = mgr
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
    assert_eq!(store.list_leased().await?.len(), 1);

    let git_ops_guard = mgr.git_ops.lock().await;
    let mgr_for_spawn = mgr.clone();
    let task_id_for_spawn = task_id.clone();
    let source_path = source.path().to_path_buf();
    let workspace_path = lease.workspace_path.clone();
    let cleanup_handle = tokio::spawn(async move {
        mgr_for_spawn
            .cleanup_workspace_for_retry(&task_id_for_spawn, &source_path, Some(&workspace_path))
            .await
    });

    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert_eq!(
        store.list_leased().await?.len(),
        1,
        "persisted lease should remain held while retry cleanup is waiting"
    );

    drop(git_ops_guard);
    tokio::time::timeout(std::time::Duration::from_secs(5), cleanup_handle)
        .await
        .expect("retry cleanup should finish")
        .expect("retry cleanup task should join")?;
    assert!(
        store.list_leased().await?.is_empty(),
        "persisted lease should release after retry cleanup finishes"
    );
    assert!(
        !lease.workspace_path.exists(),
        "retry cleanup should remove the workspace"
    );

    Ok(())
}
