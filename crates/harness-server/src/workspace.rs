use crate::task_runner::TaskId;
use dashmap::DashMap;
use harness_core::config::WorkspaceConfig;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::time::{timeout, Duration};

struct ActiveWorkspace {
    workspace_path: PathBuf,
    source_repo: PathBuf,
}

pub struct WorkspaceManager {
    pub(crate) config: WorkspaceConfig,
    active: DashMap<TaskId, ActiveWorkspace>,
}

impl WorkspaceManager {
    pub fn new(config: WorkspaceConfig) -> anyhow::Result<Self> {
        std::fs::create_dir_all(&config.root)?;
        Ok(Self {
            config,
            active: DashMap::new(),
        })
    }

    /// Create a git worktree for the given task under `config.root/<sanitized_task_id>`.
    ///
    /// Fetches `remote/base_branch` first to ensure the worktree starts from the latest
    /// upstream state. Creates branch `harness/<task_id>` based on `remote/base_branch`.
    /// Runs `after_create_hook` on new creation only. Idempotent: returns existing path
    /// if already active.
    pub async fn create_workspace(
        &self,
        task_id: &TaskId,
        source_repo: &Path,
        remote: &str,
        base_branch: &str,
    ) -> anyhow::Result<PathBuf> {
        // Validate inputs to prevent unexpected git behavior.
        if !is_valid_branch_name(base_branch) {
            anyhow::bail!("invalid base_branch: {base_branch:?}");
        }
        if !is_valid_branch_name(remote) {
            anyhow::bail!("invalid remote: {remote:?}");
        }

        // Idempotent: use entry API to avoid TOCTOU race between get and insert.
        if let Some(entry) = self.active.get(task_id) {
            return Ok(entry.workspace_path.clone());
        }

        // Fetch latest base_branch from remote so the worktree starts from upstream HEAD.
        let fetch_output = tokio::process::Command::new("git")
            .args([
                "-C",
                &source_repo.to_string_lossy(),
                "fetch",
                remote,
                base_branch,
            ])
            .output()
            .await?;

        if !fetch_output.status.success() {
            let stderr = String::from_utf8_lossy(&fetch_output.stderr);
            tracing::warn!(
                "git fetch {remote} {base_branch} failed (continuing with local): {}",
                stderr.trim()
            );
        }

        let sanitized = sanitize_task_id(&task_id.0);
        let workspace_path = self.config.root.join(&sanitized);

        // Create git worktree based on remote/base_branch (latest upstream).
        // Falls back to local base_branch if fetch failed above.
        let remote_ref = format!("{remote}/{base_branch}");
        let branch = format!("harness/{}", task_id.0);
        let output = tokio::process::Command::new("git")
            .args([
                "-C",
                &source_repo.to_string_lossy(),
                "worktree",
                "add",
                "-B",
                &branch,
                &workspace_path.to_string_lossy(),
                &remote_ref,
            ])
            .output()
            .await?;

        if !output.status.success() {
            // Fallback: try local base_branch (useful for repos without remotes, e.g. tests).
            let fallback = tokio::process::Command::new("git")
                .args([
                    "-C",
                    &source_repo.to_string_lossy(),
                    "worktree",
                    "add",
                    "-B",
                    &branch,
                    &workspace_path.to_string_lossy(),
                    base_branch,
                ])
                .output()
                .await?;

            if !fallback.status.success() {
                let stderr = String::from_utf8_lossy(&fallback.stderr);
                anyhow::bail!(
                    "git worktree add failed for task {}: {}",
                    task_id.0,
                    stderr.trim()
                );
            }
        }

        // Run after_create_hook if set. Fatal on failure: cleanup partial worktree.
        if let Some(hook) = &self.config.after_create_hook {
            let timeout_secs = self.config.hook_timeout_secs;
            let hook_error = match timeout(
                Duration::from_secs(timeout_secs),
                run_hook(hook, &workspace_path),
            )
            .await
            {
                Ok(Ok(())) => None,
                Ok(Err(e)) => Some(format!("after_create_hook failed: {e}")),
                Err(_) => Some(format!("after_create_hook timed out after {timeout_secs}s")),
            };

            if let Some(err_msg) = hook_error {
                if let Err(cleanup_err) = remove_worktree(source_repo, &workspace_path).await {
                    tracing::warn!(
                        "failed to cleanup partial worktree after hook failure: {cleanup_err}"
                    );
                }
                anyhow::bail!("{err_msg}");
            }
        }

        self.active.insert(
            task_id.clone(),
            ActiveWorkspace {
                workspace_path: workspace_path.clone(),
                source_repo: source_repo.to_path_buf(),
            },
        );
        Ok(workspace_path)
    }

    /// Remove the workspace for the given task. Runs `before_remove_hook` first (non-fatal).
    /// Idempotent: returns Ok if the task has no active workspace.
    pub async fn remove_workspace(&self, task_id: &TaskId) -> anyhow::Result<()> {
        let entry = match self.active.remove(task_id) {
            Some((_, entry)) => entry,
            None => return Ok(()),
        };

        // Run before_remove_hook if set. Non-fatal on failure.
        if let Some(hook) = &self.config.before_remove_hook {
            let timeout_secs = self.config.hook_timeout_secs;
            match timeout(
                Duration::from_secs(timeout_secs),
                run_hook(hook, &entry.workspace_path),
            )
            .await
            {
                Ok(Ok(())) => {}
                Ok(Err(e)) => tracing::warn!("before_remove_hook failed: {e}"),
                Err(_) => {
                    tracing::warn!("before_remove_hook timed out after {timeout_secs}s")
                }
            }
        }

        if let Err(e) = remove_worktree(&entry.source_repo, &entry.workspace_path).await {
            tracing::warn!(
                "git worktree remove failed for {:?}: {e}",
                entry.workspace_path
            );
        }
        Ok(())
    }

    /// Return the workspace path for the given task if it is active.
    pub fn get_workspace(&self, task_id: &TaskId) -> Option<PathBuf> {
        self.active.get(task_id).map(|e| e.workspace_path.clone())
    }

    /// Remove workspaces for all given terminal task IDs. Errors are logged, not returned.
    pub async fn cleanup_terminal(
        self: &Arc<Self>,
        terminal_task_ids: &[TaskId],
    ) -> anyhow::Result<()> {
        for task_id in terminal_task_ids {
            if let Err(e) = self.remove_workspace(task_id).await {
                tracing::warn!("cleanup_terminal: failed to remove workspace for {task_id:?}: {e}");
            }
        }
        Ok(())
    }
}

/// Validate a git branch name: must be non-empty, no whitespace, no shell metacharacters,
/// no `..`, and not start with `-`.
fn is_valid_branch_name(name: &str) -> bool {
    if name.is_empty() || name.starts_with('-') || name.contains("..") {
        return false;
    }
    name.bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'/' || b == b'-' || b == b'_' || b == b'.')
}

fn sanitize_task_id(id: &str) -> String {
    id.chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

async fn run_hook(script: &str, cwd: &Path) -> anyhow::Result<()> {
    crate::post_validator::validate_command_safety(script).map_err(|e| anyhow::anyhow!("{e}"))?;
    let output = tokio::process::Command::new("sh")
        .arg("-c")
        .arg(script)
        .current_dir(cwd)
        .output()
        .await?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!(
            "hook exited with status {:?}: {}",
            output.status.code(),
            stderr.trim()
        );
    }
    Ok(())
}

async fn remove_worktree(source_repo: &Path, workspace_path: &Path) -> anyhow::Result<()> {
    let output = tokio::process::Command::new("git")
        .args([
            "-C",
            &source_repo.to_string_lossy(),
            "worktree",
            "remove",
            "--force",
            &workspace_path.to_string_lossy(),
        ])
        .output()
        .await?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("git worktree remove failed: {}", stderr.trim());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task_runner::TaskId;

    fn init_git_repo(dir: &Path) {
        let run = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .output()
                .expect("git command failed");
        };
        run(&["-C", &dir.to_string_lossy(), "init"]);
        run(&[
            "-C",
            &dir.to_string_lossy(),
            "config",
            "user.email",
            "test@harness.test",
        ]);
        run(&[
            "-C",
            &dir.to_string_lossy(),
            "config",
            "user.name",
            "Harness Test",
        ]);
        run(&[
            "-C",
            &dir.to_string_lossy(),
            "commit",
            "--allow-empty",
            "-m",
            "init",
        ]);
    }

    fn current_branch(repo: &Path) -> String {
        let out = std::process::Command::new("git")
            .args([
                "-C",
                &repo.to_string_lossy(),
                "rev-parse",
                "--abbrev-ref",
                "HEAD",
            ])
            .output()
            .expect("git rev-parse failed");
        String::from_utf8(out.stdout)
            .expect("utf8")
            .trim()
            .to_string()
    }

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
        let task_id = TaskId("test-task-001".to_string());

        let ws_path = mgr
            .create_workspace(&task_id, source.path(), "origin", &branch)
            .await
            .expect("create");
        assert!(ws_path.is_dir());
        assert!(mgr.get_workspace(&task_id).is_some());

        mgr.remove_workspace(&task_id).await.expect("remove");
        assert!(mgr.get_workspace(&task_id).is_none());
    }

    #[tokio::test]
    async fn remove_workspace_idempotent() {
        let workspaces = tempfile::tempdir().expect("tempdir");
        let config = WorkspaceConfig {
            root: workspaces.path().to_path_buf(),
            ..Default::default()
        };
        let mgr = WorkspaceManager::new(config).expect("new");
        let task_id = TaskId("nonexistent-task".to_string());

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
        let task_id = TaskId("test-task-002".to_string());

        let path1 = mgr
            .create_workspace(&task_id, source.path(), "origin", &branch)
            .await
            .expect("create first");
        let path2 = mgr
            .create_workspace(&task_id, source.path(), "origin", &branch)
            .await
            .expect("create second");
        assert_eq!(path1, path2);

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
        let task_id = TaskId("test-task-003".to_string());

        mgr.create_workspace(&task_id, source.path(), "origin", &branch)
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
        let task_id = TaskId("test-task-004".to_string());

        let result = mgr
            .create_workspace(&task_id, source.path(), "origin", &branch)
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
            .map(|i| TaskId(format!("cleanup-task-{i:03}")))
            .collect();

        for id in &ids {
            mgr.create_workspace(id, source.path(), "origin", &branch)
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
}
