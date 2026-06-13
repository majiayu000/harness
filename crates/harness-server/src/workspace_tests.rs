use super::test_support::*;
use super::*;

#[test]
fn valid_branch_names_accepted() {
    assert!(is_valid_branch_name("main"));
    assert!(is_valid_branch_name("feature/my-branch"));
    assert!(is_valid_branch_name("release/v1.0.0"));
    assert!(is_valid_branch_name("fix_issue_42"));
}

#[test]
fn invalid_branch_names_rejected() {
    assert!(!is_valid_branch_name(""));
    assert!(!is_valid_branch_name("-starts-with-dash"));
    assert!(!is_valid_branch_name("has spaces"));
    assert!(!is_valid_branch_name("has..dotdot"));
    assert!(!is_valid_branch_name("semi;colon"));
    assert!(!is_valid_branch_name("back`tick"));
    assert!(!is_valid_branch_name("dollar$sign"));
}

#[test]
fn sanitize_task_id_replaces_non_alphanumeric() {
    assert_eq!(sanitize_task_id("abc-123"), "abc-123");
    assert_eq!(sanitize_task_id("abc_123"), "abc_123");
    assert_eq!(sanitize_task_id("abc 123"), "abc_123");
    assert_eq!(sanitize_task_id("abc/123"), "abc_123");
    assert_eq!(sanitize_task_id("abc.123"), "abc_123");
}

#[test]
fn derive_workspace_key_issue() {
    let path = std::path::Path::new("/projects/my-project");
    // canonicalize fails in test env; fnv1a_8 hashes the given path string
    let prefix = fnv1a_8("/projects/my-project");
    let key = derive_workspace_key(
        &test_task_id(),
        Some("issue:42"),
        Some("myorg/my-repo"),
        Some(path),
    );
    assert_eq!(key, format!("{prefix}__myorg_my-repo__issue_42"));
}

#[test]
fn derive_workspace_key_pr() {
    let prefix = fnv1a_8("/projects/my-project");
    let key = derive_workspace_key(
        &test_task_id(),
        Some("pr:7"),
        Some("myorg/my-repo"),
        Some(std::path::Path::new("/projects/my-project")),
    );
    assert_eq!(key, format!("{prefix}__myorg_my-repo__pr_7"));
}

#[test]
fn derive_workspace_key_prompt_falls_back_to_uuid() {
    let id = test_task_id();
    let key = derive_workspace_key(&id, None, None, None);
    assert_eq!(key, sanitize_task_id(&id.0));
}

#[test]
fn derive_workspace_key_missing_repo_falls_back() {
    let id = test_task_id();
    let key = derive_workspace_key(&id, Some("issue:42"), None, None);
    assert_eq!(key, sanitize_task_id(&id.0));
}

#[test]
fn derive_workspace_key_special_chars_in_repo() {
    // sanitize_repo_slug preserves dots: "my.org/repo name" -> "my.org_repo_name"
    // (distinct from "my_org/repo_name" -> "my_org_repo_name")
    let prefix = fnv1a_8("/projects/my-project");
    let key = derive_workspace_key(
        &test_task_id(),
        Some("issue:99"),
        Some("my.org/repo name"),
        Some(std::path::Path::new("/projects/my-project")),
    );
    assert_eq!(key, format!("{prefix}__my.org_repo_name__issue_99"));
}

#[test]
fn derive_workspace_key_no_source_repo_omits_prefix() {
    let key = derive_workspace_key(
        &test_task_id(),
        Some("issue:42"),
        Some("myorg/my-repo"),
        None,
    );
    assert_eq!(key, "myorg_my-repo__issue_42");
}

#[test]
fn sanitize_repo_slug_preserves_dots() {
    assert_eq!(sanitize_repo_slug("my.org/my-repo"), "my.org_my-repo");
    assert_eq!(sanitize_repo_slug("my_org/my-repo"), "my_org_my-repo");
    // dots and underscores produce distinct keys
    assert_ne!(
        sanitize_repo_slug("my.org/repo"),
        sanitize_repo_slug("my_org/repo")
    );
}

#[test]
fn derive_workspace_key_different_projects_same_dirname_differ() {
    // Two projects with the same dir name but different parent paths must
    // produce different workspace keys (hash of full path, not just file_name).
    let key_a = derive_workspace_key(
        &test_task_id(),
        Some("issue:1"),
        Some("org/repo"),
        Some(std::path::Path::new("/home/user/app")),
    );
    let key_b = derive_workspace_key(
        &test_task_id(),
        Some("issue:1"),
        Some("org/repo"),
        Some(std::path::Path::new("/opt/app")),
    );
    assert_ne!(
        key_a, key_b,
        "projects at different paths must produce different keys"
    );
}

#[test]
fn workspace_manager_new_creates_root_dir() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().join("workspaces");
    let config = WorkspaceConfig {
        root: root.clone(),
        ..Default::default()
    };
    let _mgr = WorkspaceManager::new(config).expect("new");
    assert!(root.is_dir());
}

#[tokio::test]
async fn create_and_remove_workspace() {
    let source = tempfile::tempdir().expect("tempdir");
    init_git_repo(source.path());
    let branch = current_branch(source.path());

    let workspaces = tempfile::tempdir().expect("tempdir");
    let config = WorkspaceConfig {
        root: workspaces.path().to_path_buf(),
        auto_cleanup: true,
        ..Default::default()
    };
    let mgr = WorkspaceManager::new(config).expect("new");
    let task_id = harness_core::types::TaskId("test-task-001".to_string());

    let ws_path = mgr
        .create_workspace(&task_id, source.path(), "origin", &branch, 1, None, None)
        .await
        .expect("create");
    assert!(ws_path.workspace_path.is_dir());
    assert!(mgr.get_workspace(&task_id).is_some());

    mgr.remove_workspace(&task_id).await.expect("remove");
    assert!(mgr.get_workspace(&task_id).is_none());
}

#[tokio::test]
async fn create_workspace_persists_owner_record_outside_checkout_root() {
    let source = tempfile::tempdir().expect("tempdir");
    init_git_repo(source.path());
    let branch = current_branch(source.path());

    let workspaces = tempfile::tempdir().expect("tempdir");
    let config = WorkspaceConfig {
        root: workspaces.path().to_path_buf(),
        ..Default::default()
    };
    let mgr = WorkspaceManager::new(config).expect("new");
    let task_id = harness_core::types::TaskId("test-task-owner-record".to_string());

    let lease = mgr
        .create_workspace(&task_id, source.path(), "origin", &branch, 1, None, None)
        .await
        .expect("create");

    let metadata_path = owner_record_path(&lease.workspace_path).expect("owner record path");
    assert!(
        metadata_path.exists(),
        "owner record should be written into git metadata"
    );
    assert!(
        !lease
            .workspace_path
            .join(format!(".{OWNER_RECORD_FILE}"))
            .exists()
            && !lease.workspace_path.join(OWNER_RECORD_FILE).exists(),
        "owner record must not dirty the checkout root"
    );

    mgr.remove_workspace(&task_id).await.expect("remove");
}

#[tokio::test]
async fn cleanup_workspace_for_retry_skips_path_reserved_by_different_active_task() {
    let source = tempfile::tempdir().expect("tempdir");
    init_git_repo(source.path());
    let branch = current_branch(source.path());

    let workspaces = tempfile::tempdir().expect("tempdir");
    let config = WorkspaceConfig {
        root: workspaces.path().to_path_buf(),
        ..Default::default()
    };
    let mgr = WorkspaceManager::new(config).expect("new");
    let active_task = harness_core::types::TaskId("active-issue-task".to_string());
    let stale_task = harness_core::types::TaskId("stale-retry-task".to_string());

    let lease = mgr
        .create_workspace(
            &active_task,
            source.path(),
            "origin",
            &branch,
            1,
            Some("issue:42"),
            Some("owner/repo"),
        )
        .await
        .expect("create active workspace");

    mgr.cleanup_workspace_for_retry(&stale_task, source.path(), Some(&lease.workspace_path))
        .await
        .expect("retry cleanup should skip foreign active path");

    assert!(
        lease.workspace_path.exists(),
        "retry cleanup must not delete another active task's workspace"
    );
    assert!(
        mgr.get_workspace(&active_task).is_some(),
        "active task should remain tracked"
    );

    mgr.remove_workspace(&active_task)
        .await
        .expect("remove active");
}

#[tokio::test]
async fn remove_workspace_idempotent() {
    let workspaces = tempfile::tempdir().expect("tempdir");
    let config = WorkspaceConfig {
        root: workspaces.path().to_path_buf(),
        ..Default::default()
    };
    let mgr = WorkspaceManager::new(config).expect("new");
    let task_id = harness_core::types::TaskId("nonexistent-task".to_string());

    // Should succeed even though workspace was never created.
    mgr.remove_workspace(&task_id).await.expect("first remove");
    mgr.remove_workspace(&task_id).await.expect("second remove");
}

#[tokio::test]
async fn create_workspace_idempotent() {
    let source = tempfile::tempdir().expect("tempdir");
    init_git_repo(source.path());
    let branch = current_branch(source.path());

    let workspaces = tempfile::tempdir().expect("tempdir");
    let config = WorkspaceConfig {
        root: workspaces.path().to_path_buf(),
        ..Default::default()
    };
    let mgr = WorkspaceManager::new(config).expect("new");
    let task_id = harness_core::types::TaskId("test-task-002".to_string());

    let path1 = mgr
        .create_workspace(&task_id, source.path(), "origin", &branch, 1, None, None)
        .await
        .expect("create first");
    let path2 = mgr
        .create_workspace(&task_id, source.path(), "origin", &branch, 1, None, None)
        .await
        .expect("create second");
    assert_eq!(path1.workspace_path, path2.workspace_path);

    mgr.remove_workspace(&task_id).await.expect("remove");
}

#[tokio::test]
async fn create_workspace_ignores_inherited_git_index_file() {
    let _guard = ScopedEnvVar::set("GIT_INDEX_FILE", ".git/index");

    let source = tempfile::tempdir().expect("tempdir");
    init_git_repo(source.path());
    let branch = current_branch(source.path());

    let workspaces = tempfile::tempdir().expect("tempdir");
    let config = WorkspaceConfig {
        root: workspaces.path().to_path_buf(),
        ..Default::default()
    };
    let mgr = WorkspaceManager::new(config).expect("new");
    let task_id = harness_core::types::TaskId("test-task-git-index-file".to_string());

    let ws_path = mgr
        .create_workspace(&task_id, source.path(), "origin", &branch, 1, None, None)
        .await
        .expect("create");
    assert!(ws_path.workspace_path.is_dir());

    mgr.remove_workspace(&task_id).await.expect("remove");
}

#[tokio::test]
async fn create_workspace_requires_remote_head_by_default() {
    let source = tempfile::tempdir().expect("tempdir");
    init_git_repo(source.path());
    let branch = current_branch(source.path());
    run_git(&[
        "-C",
        &source.path().to_string_lossy(),
        "remote",
        "remove",
        "origin",
    ]);

    let workspaces = tempfile::tempdir().expect("tempdir");
    let config = WorkspaceConfig {
        root: workspaces.path().to_path_buf(),
        ..Default::default()
    };
    let mgr = WorkspaceManager::new(config).expect("new");
    let task_id = harness_core::types::TaskId("missing-remote-head".to_string());

    let err = mgr
        .create_workspace(&task_id, source.path(), "origin", &branch, 1, None, None)
        .await
        .expect_err("missing remote head should fail admission");
    assert!(
        err.to_string().contains("git fetch origin"),
        "error should report the failed remote fetch: {err}"
    );
    assert!(mgr.get_workspace(&task_id).is_none());
}

#[tokio::test]
async fn create_workspace_can_opt_into_local_base_fallback() {
    let source = tempfile::tempdir().expect("tempdir");
    init_git_repo(source.path());
    let branch = current_branch(source.path());
    run_git(&[
        "-C",
        &source.path().to_string_lossy(),
        "remote",
        "remove",
        "origin",
    ]);

    let workspaces = tempfile::tempdir().expect("tempdir");
    let config = WorkspaceConfig {
        root: workspaces.path().to_path_buf(),
        ..Default::default()
    };
    let mgr = WorkspaceManager::new(config).expect("new");
    let task_id = harness_core::types::TaskId("local-base-fallback".to_string());

    let lease = mgr
        .create_workspace_with_options(
            &task_id,
            source.path(),
            "origin",
            &branch,
            1,
            None,
            None,
            WorkspaceCreateOptions {
                require_remote_head: false,
                ..Default::default()
            },
        )
        .await
        .expect("explicit local fallback should be allowed");
    assert!(lease.workspace_path.is_dir());

    mgr.remove_workspace(&task_id).await.expect("remove");
}

#[tokio::test]
async fn create_workspace_uses_custom_branch_prefix() {
    let source = tempfile::tempdir().expect("tempdir");
    init_git_repo(source.path());
    let branch = current_branch(source.path());

    let workspaces = tempfile::tempdir().expect("tempdir");
    let config = WorkspaceConfig {
        root: workspaces.path().to_path_buf(),
        ..Default::default()
    };
    let mgr = WorkspaceManager::new(config).expect("new");
    let task_id = harness_core::types::TaskId("custom-prefix".to_string());

    let lease = mgr
        .create_workspace_with_options(
            &task_id,
            source.path(),
            "origin",
            &branch,
            1,
            None,
            None,
            WorkspaceCreateOptions {
                branch_prefix: "task/".to_string(),
                ..Default::default()
            },
        )
        .await
        .expect("workspace should be created with custom branch prefix");
    assert_eq!(current_branch(&lease.workspace_path), "task/custom-prefix");

    mgr.remove_workspace(&task_id).await.expect("remove");
}

#[tokio::test]
async fn after_create_hook_runs_with_workspace_cwd() {
    let source = tempfile::tempdir().expect("tempdir");
    init_git_repo(source.path());
    let branch = current_branch(source.path());

    let workspaces = tempfile::tempdir().expect("tempdir");
    let marker = workspaces.path().join("hook_ran.marker");
    let hook = format!("touch {}", marker.display());

    let config = WorkspaceConfig {
        root: workspaces.path().to_path_buf(),
        after_create_hook: Some(hook),
        ..Default::default()
    };
    let mgr = WorkspaceManager::new(config).expect("new");
    let task_id = harness_core::types::TaskId("test-task-003".to_string());

    mgr.create_workspace(&task_id, source.path(), "origin", &branch, 1, None, None)
        .await
        .expect("create");
    assert!(
        marker.exists(),
        "after_create_hook should have created marker file"
    );

    mgr.remove_workspace(&task_id).await.expect("remove");
}

#[tokio::test]
async fn after_create_hook_failure_removes_partial_worktree() {
    let source = tempfile::tempdir().expect("tempdir");
    init_git_repo(source.path());
    let branch = current_branch(source.path());

    let workspaces = tempfile::tempdir().expect("tempdir");
    let config = WorkspaceConfig {
        root: workspaces.path().to_path_buf(),
        after_create_hook: Some("exit 1".to_string()),
        ..Default::default()
    };
    let mgr = WorkspaceManager::new(config).expect("new");
    let task_id = harness_core::types::TaskId("test-task-004".to_string());

    let result = mgr
        .create_workspace(&task_id, source.path(), "origin", &branch, 1, None, None)
        .await;
    assert!(result.is_err(), "should fail when hook exits 1");
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("after_create_hook"),
        "error should mention hook"
    );
    // Should not be tracked in active map.
    assert!(mgr.get_workspace(&task_id).is_none());
}

#[tokio::test]
async fn create_workspace_reconciles_stale_directory() {
    let source = tempfile::tempdir().expect("tempdir");
    init_git_repo(source.path());
    let branch = current_branch(source.path());

    let workspaces = tempfile::tempdir().expect("tempdir");
    let config = WorkspaceConfig {
        root: workspaces.path().to_path_buf(),
        ..Default::default()
    };
    let mgr = WorkspaceManager::new(config).expect("new");
    let task_id = harness_core::types::TaskId("stale-task-check-001".to_string());

    // Pre-create the directory to simulate a stale worktree from a previous failed run.
    let pool_key = crate::workspace_pool::derive_workspace_pool_key(source.path(), None);
    let stale_path = workspaces
        .path()
        .join(crate::workspace_pool::workspace_slot_key(&pool_key, 0));
    std::fs::create_dir_all(&stale_path).expect("create stale dir");

    let result = mgr
        .create_workspace(&task_id, source.path(), "origin", &branch, 1, None, None)
        .await;
    assert!(
        result.is_ok(),
        "stale directory should be reconciled automatically"
    );
    assert!(
        stale_path.join(".git").exists(),
        "workspace should be recreated as a git worktree"
    );
    assert!(
        mgr.get_workspace(&task_id).is_some(),
        "task should be tracked after reconciliation"
    );
}

#[tokio::test]
async fn create_workspace_reconciles_missing_registered_worktree() {
    let source = tempfile::tempdir().expect("tempdir");
    init_git_repo(source.path());
    let branch = current_branch(source.path());

    let workspaces = tempfile::tempdir().expect("tempdir");
    let config = WorkspaceConfig {
        root: workspaces.path().to_path_buf(),
        ..Default::default()
    };
    let task_id = harness_core::types::TaskId("missing-registered-task".to_string());
    let first_mgr = WorkspaceManager::new(config.clone()).expect("first manager");
    let first = first_mgr
        .create_workspace(&task_id, source.path(), "origin", &branch, 1, None, None)
        .await
        .expect("create initial workspace");
    std::fs::remove_dir_all(&first.workspace_path).expect("remove worktree directory");
    assert!(
        is_registered_worktree(source.path(), &first.workspace_path).await,
        "removed workspace path should still be registered in git metadata"
    );

    let second_mgr = WorkspaceManager::new(config).expect("second manager");
    let recreated = second_mgr
        .create_workspace(&task_id, source.path(), "origin", &branch, 1, None, None)
        .await
        .expect("missing registered worktree should be pruned and recreated");

    assert_eq!(recreated.decision, WorkspaceAcquireDecision::RecreatedStale);
    assert!(recreated.workspace_path.exists());
    assert!(
        is_registered_worktree(source.path(), &recreated.workspace_path).await,
        "recreated workspace should be registered as a live worktree"
    );
}

#[tokio::test]
async fn create_workspace_blocks_live_foreign_owner() {
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
    let task_id = harness_core::types::TaskId("foreign-owner-task".to_string());

    mgr_a
        .create_workspace(&task_id, source.path(), "origin", &branch, 1, None, None)
        .await
        .expect("create first owner");

    let err = mgr_b
        .create_workspace(&task_id, source.path(), "origin", &branch, 1, None, None)
        .await
        .expect_err("second owner should be blocked");
    assert!(
        err.to_string().contains("manual resolution required"),
        "foreign live owner should remain a protected hard stop: {err}"
    );
}
