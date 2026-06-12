use super::*;

#[tokio::test]
async fn terminal_runtime_cleanup_releases_missing_workspace_without_git_cleanup(
) -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let project_root = dir.path().join("project-missing-terminal-workspace");
    std::fs::create_dir_all(&project_root)?;
    std::fs::write(
        project_root.join("WORKFLOW.md"),
        "---\nbase:\n  require_remote_head: false\nworkspace:\n  strategy: worktree\n  cleanup: on_terminal\n  reuse_existing_workspace: true\n---\n",
    )?;
    init_worktree_git_repo(&project_root)?;
    let workspace_root = dir.path().join("workspaces");
    let mut config = harness_core::config::HarnessConfig::default();
    config.workspace.root = workspace_root;
    config.workspace.root_configured = true;
    let agent = RuntimeStreamAgent::new();
    let mut registry = harness_agents::registry::AgentRegistry::new("codex");
    registry.register("codex", agent);
    let state = make_test_state_with_workflow_runtime_config_and_registry(
        dir.path(),
        &project_root,
        config.clone(),
        registry,
    )
    .await?;
    let workspace_mgr = Arc::new(crate::workspace::WorkspaceManager::new(
        config.workspace.clone(),
    )?);
    let mut state = match Arc::try_unwrap(state) {
        Ok(state) => state,
        Err(_) => panic!("test state should have one owner before workspace manager injection"),
    };
    state.concurrency.workspace_mgr = Some(workspace_mgr.clone());
    let state = Arc::new(state);
    let store = state
        .core
        .workflow_runtime_store
        .as_ref()
        .expect("workflow runtime store should be configured");
    let workflow = harness_workflow::runtime::WorkflowInstance::new(
        "github_issue_pr",
        1,
        "failed",
        harness_workflow::runtime::WorkflowSubject::new("issue", "issue:1299"),
    )
    .with_id("issue-1299-missing-workspace")
    .with_data(serde_json::json!({
        "project_id": project_root,
        "repo": "owner/repo",
        "issue_number": 1299,
    }));
    store.upsert_instance(&workflow).await?;
    let task_id = crate::task_runner::TaskId::from_str(&format!(
        "runtime-wf-github-issue-pr-{}",
        stable_hash_8_for_test(&workflow.id)
    ));
    let workspace_path = workspace_mgr.workspace_path_for(
        &task_id,
        &project_root,
        Some(workflow.subject.subject_key.as_str()),
        Some("owner/repo"),
    );
    workspace_mgr.active.insert(
        task_id.clone(),
        crate::workspace::ActiveWorkspace {
            workspace_path: workspace_path.clone(),
            source_repo: project_root.clone(),
            repo: Some("owner/repo".to_string()),
            runtime_workflow_id: Some(workflow.id.clone()),
            project_key: "test-project".to_string(),
            slot_index: 0,
            branch: "harness/runtime-wf-github-issue-pr-test".to_string(),
            created_at: std::time::SystemTime::now(),
            owner_session: workspace_mgr.owner_session.clone(),
            run_generation: 1,
            _pool_permit: None,
        },
    );
    workspace_mgr
        .active_paths
        .insert(workspace_path.clone(), task_id.clone());
    assert!(!workspace_path.exists());
    assert_eq!(workspace_mgr.live_count(), 1);
    let marker = dir.path().join("git-cleanup-marker");
    let proxy = write_git_marker_proxy(
        dir.path(),
        &project_root.to_string_lossy(),
        &marker.to_string_lossy(),
    )?;
    let previous_git_bin = set_missing_workspace_test_git_proxy(proxy.as_os_str());

    let cleanup_result =
        crate::workflow_runtime_worker::notify_runtime_submission_terminal_workflow(
            &state,
            &workflow.id,
            None,
        )
        .await;
    restore_missing_workspace_test_git_proxy(previous_git_bin);
    cleanup_result?;

    assert!(
        !marker.exists(),
        "missing terminal workspace cleanup should release the lease without invoking git cleanup"
    );
    assert_eq!(workspace_mgr.live_count(), 0);
    assert!(workspace_mgr.get_workspace(&task_id).is_none());
    assert!(
        workspace_mgr.active_paths.get(&workspace_path).is_none(),
        "missing workspace release should clear the active path reservation"
    );
    Ok(())
}

fn stable_hash_8_for_test(value: &str) -> String {
    let mut hash: u32 = 0x811c9dc5;
    for byte in value.bytes() {
        hash ^= u32::from(byte);
        hash = hash.wrapping_mul(0x01000193);
    }
    format!("{hash:08x}")
}

fn write_git_marker_proxy(
    root: &std::path::Path,
    project_root: &str,
    marker: &str,
) -> anyhow::Result<std::path::PathBuf> {
    #[cfg(not(unix))]
    anyhow::bail!("git marker proxy test requires a unix shell");
    #[cfg(unix)]
    {
        let proxy = root.join("git-marker-proxy.sh");
        let real_git = std::env::var("HARNESS_GIT_BIN").unwrap_or_else(|_| "git".to_string());
        std::fs::write(
            &proxy,
            format!(
                "#!/bin/sh\nproject_root={}\nmarker={}\nreal_git={}\nif printf '%s\\n' \"$*\" | grep -F -- \"$project_root\" >/dev/null; then\n  printf '%s\\n' \"$*\" >> \"$marker\"\nfi\nexec \"$real_git\" \"$@\"\n",
                shell_quote(project_root),
                shell_quote(marker),
                shell_quote(&real_git)
            ),
        )?;
        let mut permissions = std::fs::metadata(&proxy)?.permissions();
        use std::os::unix::fs::PermissionsExt;
        permissions.set_mode(0o755);
        std::fs::set_permissions(&proxy, permissions)?;
        Ok(proxy)
    }
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn set_missing_workspace_test_git_proxy(value: &std::ffi::OsStr) -> Option<std::ffi::OsString> {
    let previous = std::env::var_os("HARNESS_GIT_BIN");
    // SAFETY: this scoped test override is restored before assertions; the proxy
    // delegates every git command to the real binary and only records calls for
    // this test's private temporary project path.
    unsafe { std::env::set_var("HARNESS_GIT_BIN", value) };
    previous
}

fn restore_missing_workspace_test_git_proxy(previous: Option<std::ffi::OsString>) {
    match previous {
        Some(value) => {
            // SAFETY: restores the process environment value saved by this test.
            unsafe { std::env::set_var("HARNESS_GIT_BIN", value) };
        }
        None => {
            // SAFETY: restores absence for the process environment value saved by this test.
            unsafe { std::env::remove_var("HARNESS_GIT_BIN") };
        }
    }
}
