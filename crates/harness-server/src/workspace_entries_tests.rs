use super::*;
use harness_core::types::TaskId;

#[test]
fn workspace_entries_return_active_workspace_metadata_sorted_by_task_id() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().join("workspaces");
    let mgr = WorkspaceManager::new(WorkspaceConfig {
        root: root.clone(),
        ..Default::default()
    })
    .expect("new");
    let created_at = std::time::UNIX_EPOCH + std::time::Duration::from_secs(42);

    for task_id in ["task-b", "task-a"] {
        let task_id = TaskId(task_id.to_string());
        let workspace_path = root.join(&task_id.0);
        let source_repo = tmp.path().join("repo");
        mgr.active.insert(
            task_id.clone(),
            ActiveWorkspace {
                workspace_path: workspace_path.clone(),
                source_repo: source_repo.clone(),
                repo: Some("owner/repo".to_string()),
                runtime_workflow_id: Some("workflow-1".to_string()),
                branch: format!("harness/{}", task_id.0),
                created_at,
                owner_session: mgr.owner_session.clone(),
                run_generation: 1,
            },
        );
        mgr.active_paths.insert(workspace_path, task_id);
    }

    let entries = mgr.entries();

    assert_eq!(
        entries
            .iter()
            .map(|entry| entry.task_id.0.as_str())
            .collect::<Vec<_>>(),
        vec!["task-a", "task-b"]
    );
    assert_eq!(entries[0].workspace_path, root.join("task-a"));
    assert_eq!(entries[0].source_repo, tmp.path().join("repo"));
    assert_eq!(entries[0].repo.as_deref(), Some("owner/repo"));
    assert_eq!(
        entries[0].runtime_workflow_id.as_deref(),
        Some("workflow-1")
    );
    assert_eq!(entries[0].branch, "harness/task-a");
    assert_eq!(entries[0].created_at, created_at);
}
