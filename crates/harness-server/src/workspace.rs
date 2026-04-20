use crate::task_runner::TaskId;
use dashmap::DashMap;
use harness_core::config::misc::WorkspaceConfig;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::time::{timeout, Duration};

/// Git hook invocations inherit repository-local environment variables such as
/// `GIT_INDEX_FILE=.git/index`. Those paths are valid for the original checkout
/// but break nested `git worktree` operations because worktrees use a `.git`
/// file instead of a directory. Strip the local Git env before spawning any
/// git subprocess for workspace management or test repo setup.
const GIT_LOCAL_ENV_VARS: &[&str] = &[
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_CONFIG",
    "GIT_CONFIG_PARAMETERS",
    "GIT_CONFIG_COUNT",
    "GIT_OBJECT_DIRECTORY",
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_IMPLICIT_WORK_TREE",
    "GIT_GRAFT_FILE",
    "GIT_INDEX_FILE",
    "GIT_NO_REPLACE_OBJECTS",
    "GIT_REPLACE_REF_BASE",
    "GIT_PREFIX",
    "GIT_SHALLOW_FILE",
    "GIT_COMMON_DIR",
];

fn git_command() -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new("git");
    for key in GIT_LOCAL_ENV_VARS {
        cmd.env_remove(key);
    }
    cmd
}

pub(crate) struct ActiveWorkspace {
    pub(crate) workspace_path: PathBuf,
    pub(crate) source_repo: PathBuf,
}

pub struct WorkspaceManager {
    pub(crate) config: WorkspaceConfig,
    pub(crate) active: DashMap<TaskId, ActiveWorkspace>,
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

        let sanitized = sanitize_task_id(&task_id.0);
        let workspace_path = self.config.root.join(&sanitized);

        // Atomic check-and-insert: prevents TOCTOU where two concurrent calls for the
        // same task_id would both attempt git worktree add on the same path.
        // Insert a placeholder immediately so any concurrent caller returns early.
        {
            use dashmap::mapref::entry::Entry;
            match self.active.entry(task_id.clone()) {
                Entry::Occupied(occ) => return Ok(occ.get().workspace_path.clone()),
                Entry::Vacant(vac) => {
                    vac.insert(ActiveWorkspace {
                        workspace_path: workspace_path.clone(),
                        source_repo: source_repo.to_path_buf(),
                    });
                }
            }
        }

        // Stale-state guard: if the workspace directory already exists on disk but was
        // not tracked as active (Occupied path above), check whether it is this task's
        // own worktree left behind by a server crash (recovery path) or a truly foreign
        // worktree (collision path).
        if workspace_path.exists() {
            let expected_branch = format!("harness/{}", task_id.0);
            if is_worktree_on_branch(&workspace_path, &expected_branch).await {
                // Recovery: same task's worktree survived a crash — re-register and reuse.
                tracing::info!(
                    task_id = %task_id.0,
                    path = ?workspace_path,
                    "workspace recovery: re-using existing worktree from previous run"
                );
                return Ok(workspace_path);
            }
            self.active.remove(task_id);
            anyhow::bail!(
                "WorktreeCollision: workspace path {:?} already exists on disk for task {}; \
                 a previous run did not clean up — remove the directory to retry",
                workspace_path,
                task_id.0
            );
        }

        // Fetch latest base_branch from remote so the worktree starts from upstream HEAD.
        let fetch_output = git_command()
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

        // Create git worktree based on remote/base_branch (latest upstream).
        // Falls back to local base_branch if fetch failed above.
        let remote_ref = format!("{remote}/{base_branch}");
        let branch = format!("harness/{}", task_id.0);
        let output = git_command()
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
            let fallback = git_command()
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
                self.active.remove(task_id);
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
                self.active.remove(task_id);
                if let Err(cleanup_err) = remove_worktree(source_repo, &workspace_path).await {
                    tracing::warn!(
                        "failed to cleanup partial worktree after hook failure: {cleanup_err}"
                    );
                }
                anyhow::bail!("{err_msg}");
            }
        }

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

    /// Number of worktrees currently checked out and not yet reaped.
    pub fn live_count(&self) -> u64 {
        self.active.len() as u64
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

    /// Scan `config.root` for worktree directories and remove any that correspond to
    /// terminal (Done/Failed) task IDs and are not currently tracked as active.
    /// This cleans up orphaned worktrees left behind by a previous server crash.
    ///
    /// Errors from individual removals are logged and do not abort the sweep.
    pub async fn cleanup_orphan_worktrees(&self, source_repo: &Path, terminal_task_ids: &[TaskId]) {
        let terminal_dirs: std::collections::HashSet<String> = terminal_task_ids
            .iter()
            .map(|id| sanitize_task_id(&id.0))
            .collect();

        let read_dir = match std::fs::read_dir(&self.config.root) {
            Ok(rd) => rd,
            Err(e) => {
                tracing::warn!(
                    "cleanup_orphan_worktrees: failed to read workspace root {:?}: {e}",
                    self.config.root
                );
                return;
            }
        };

        for entry in read_dir.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let dir_name = match path.file_name().and_then(|n| n.to_str()) {
                Some(n) => n.to_string(),
                None => continue,
            };
            // Match the exact task ID or a known derived sub-workspace suffix:
            //   `{task}-seq`   — sequential run workspace
            //   `{task}-p{N}`  — parallel chunk workspace (N = decimal digits only)
            // A broad `starts_with("{td}-")` would also match unrelated workspaces
            // like `task-42-hotfix`, incorrectly deleting them when `task-42` is
            // terminal.  Restricting to the two known suffixes prevents false positives.
            let is_terminal = terminal_dirs.iter().any(|td| {
                dir_name == *td
                    || dir_name == format!("{td}-seq")
                    || dir_name
                        .strip_prefix(&format!("{td}-p"))
                        .is_some_and(|rest| {
                            !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit())
                        })
            });
            if !is_terminal {
                continue;
            }
            if self.active.iter().any(|e| e.workspace_path == path) {
                continue;
            }
            tracing::info!(
                "cleanup_orphan_worktrees: removing orphan worktree {:?}",
                path
            );
            if let Err(e) = remove_worktree(source_repo, &path).await {
                tracing::warn!("cleanup_orphan_worktrees: failed to remove {:?}: {e}", path);
            }
        }

        // Prune stale worktree metadata so git no longer tracks removed directories.
        if let Err(e) = git_command()
            .args(["-C", &source_repo.to_string_lossy(), "worktree", "prune"])
            .output()
            .await
        {
            tracing::warn!("cleanup_orphan_worktrees: git worktree prune failed: {e}");
        }
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

/// Returns true when the git worktree at `path` is currently on `branch`.
/// Used to distinguish crash-recovery (same task's worktree) from a true collision.
async fn is_worktree_on_branch(path: &Path, branch: &str) -> bool {
    git_command()
        .args([
            "-C",
            &path.to_string_lossy(),
            "rev-parse",
            "--abbrev-ref",
            "HEAD",
        ])
        .output()
        .await
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|b| b.trim() == branch)
        .unwrap_or(false)
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
    let output = git_command()
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
        if workspace_path.exists() {
            tracing::warn!(
                "orphan workspace {:?} is not a git worktree — delete it manually: rm -rf {:?}",
                workspace_path,
                workspace_path
            );
        }
        anyhow::bail!("git worktree remove failed: {}", stderr.trim());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task_runner::TaskId;
    use std::sync::{Mutex, MutexGuard, OnceLock};

    fn git_command_std() -> std::process::Command {
        let mut cmd = std::process::Command::new("git");
        for key in GIT_LOCAL_ENV_VARS {
            cmd.env_remove(key);
        }
        cmd
    }

    fn env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    struct ScopedEnvVar {
        key: String,
        original: Option<String>,
        _guard: MutexGuard<'static, ()>,
    }

    impl ScopedEnvVar {
        fn set(key: &str, value: &str) -> Self {
            let guard = env_lock().lock().expect("env lock should not be poisoned");
            let original = std::env::var(key).ok();
            unsafe { std::env::set_var(key, value) };
            Self {
                key: key.to_string(),
                original,
                _guard: guard,
            }
        }
    }

    impl Drop for ScopedEnvVar {
        fn drop(&mut self) {
            if let Some(value) = &self.original {
                unsafe { std::env::set_var(&self.key, value) };
            } else {
                unsafe { std::env::remove_var(&self.key) };
            }
        }
    }

    fn run_git(args: &[&str]) -> std::process::Output {
        let output = git_command_std()
            .args(args)
            .output()
            .expect("git command failed to spawn");
        assert!(
            output.status.success(),
            "git command failed: args={args:?}, stderr={}",
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }

    fn init_git_repo(dir: &Path) {
        let run = |args: &[&str]| {
            run_git(args);
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
        let out = run_git(&[
            "-C",
            &repo.to_string_lossy(),
            "rev-parse",
            "--abbrev-ref",
            "HEAD",
        ]);
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
        let task_id = harness_core::types::TaskId("test-task-001".to_string());

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
            .create_workspace(&task_id, source.path(), "origin", &branch)
            .await
            .expect("create");
        assert!(ws_path.is_dir());

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
        let task_id = harness_core::types::TaskId("test-task-004".to_string());

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
    async fn create_workspace_rejects_stale_directory() {
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
        let stale_path = workspaces.path().join("stale-task-check-001");
        std::fs::create_dir_all(&stale_path).expect("create stale dir");

        let result = mgr
            .create_workspace(&task_id, source.path(), "origin", &branch)
            .await;
        assert!(result.is_err(), "should fail when stale directory exists");
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("WorktreeCollision"),
            "error should mention WorktreeCollision, got: {err_msg}"
        );
        // Task must not be left in the active map after a collision error.
        assert!(
            mgr.get_workspace(&task_id).is_none(),
            "task should not be in active map after collision"
        );
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
