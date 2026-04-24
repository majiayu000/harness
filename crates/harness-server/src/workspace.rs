use crate::task_runner::{TaskId, TaskSummary};
use dashmap::DashMap;
use harness_core::config::misc::WorkspaceConfig;
use harness_core::types::SessionId;
use serde::{Deserialize, Serialize};
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

const OWNER_RECORD_FILE: &str = "harness-workspace-owner.json";

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
    pub(crate) owner_session: String,
    pub(crate) run_generation: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct WorkspaceOwnerRecord {
    task_id: String,
    run_generation: u32,
    owner_session: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum WorkspaceAcquireDecision {
    CreatedFresh,
    ReusedTracked,
    ReusedRecovered,
    RecreatedStale,
}

#[derive(Debug, Clone)]
pub(crate) struct WorkspaceLease {
    pub(crate) workspace_path: PathBuf,
    pub(crate) owner_session: String,
    pub(crate) run_generation: u32,
    pub(crate) decision: WorkspaceAcquireDecision,
}

#[derive(Debug, Clone)]
pub(crate) enum WorkspaceLifecycleError {
    LiveForeignOwner {
        workspace_path: PathBuf,
        workspace_owner: Option<String>,
        message: String,
    },
    ReconcileFailed {
        workspace_path: PathBuf,
        workspace_owner: Option<String>,
        message: String,
    },
    CreateFailed {
        workspace_path: PathBuf,
        workspace_owner: Option<String>,
        message: String,
    },
}

impl WorkspaceLifecycleError {
    pub(crate) fn workspace_path(&self) -> &Path {
        match self {
            Self::LiveForeignOwner { workspace_path, .. }
            | Self::ReconcileFailed { workspace_path, .. }
            | Self::CreateFailed { workspace_path, .. } => workspace_path.as_path(),
        }
    }

    pub(crate) fn workspace_owner(&self) -> Option<&str> {
        match self {
            Self::LiveForeignOwner {
                workspace_owner, ..
            }
            | Self::ReconcileFailed {
                workspace_owner, ..
            }
            | Self::CreateFailed {
                workspace_owner, ..
            } => workspace_owner.as_deref(),
        }
    }
}

impl std::fmt::Display for WorkspaceLifecycleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::LiveForeignOwner { message, .. }
            | Self::ReconcileFailed { message, .. }
            | Self::CreateFailed { message, .. } => f.write_str(message),
        }
    }
}

impl std::error::Error for WorkspaceLifecycleError {}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct StartupReconciliation {
    pub(crate) removed: u32,
    pub(crate) preserved: u32,
}

pub struct WorkspaceManager {
    pub(crate) config: WorkspaceConfig,
    pub(crate) active: DashMap<TaskId, ActiveWorkspace>,
    pub(crate) owner_session: String,
}

impl WorkspaceManager {
    pub fn new(mut config: WorkspaceConfig) -> anyhow::Result<Self> {
        if !config.root.is_absolute() {
            config.root = std::env::current_dir()?.join(&config.root);
        }
        std::fs::create_dir_all(&config.root)?;
        Ok(Self {
            config,
            active: DashMap::new(),
            owner_session: SessionId::new().to_string(),
        })
    }

    /// Create a git worktree for the given task under `config.root/<sanitized_task_id>`.
    ///
    /// Fetches `remote/base_branch` first to ensure the worktree starts from the latest
    /// upstream state. Creates branch `harness/<task_id>` based on `remote/base_branch`.
    /// Runs `after_create_hook` on new creation only. Idempotent: returns existing path
    /// if already active.
    pub(crate) async fn create_workspace(
        &self,
        task_id: &TaskId,
        source_repo: &Path,
        remote: &str,
        base_branch: &str,
        run_generation: u32,
    ) -> Result<WorkspaceLease, WorkspaceLifecycleError> {
        // Validate inputs to prevent unexpected git behavior.
        if !is_valid_branch_name(base_branch) {
            return Err(WorkspaceLifecycleError::CreateFailed {
                workspace_path: self.config.root.join(sanitize_task_id(&task_id.0)),
                workspace_owner: None,
                message: format!("invalid base_branch: {base_branch:?}"),
            });
        }
        if !is_valid_branch_name(remote) {
            return Err(WorkspaceLifecycleError::CreateFailed {
                workspace_path: self.config.root.join(sanitize_task_id(&task_id.0)),
                workspace_owner: None,
                message: format!("invalid remote: {remote:?}"),
            });
        }

        let sanitized = sanitize_task_id(&task_id.0);
        let workspace_path = self.config.root.join(&sanitized);
        let owner_session = self.owner_session.clone();

        // Atomic check-and-insert: prevents TOCTOU where two concurrent calls for the
        // same task_id would both attempt git worktree add on the same path.
        // Insert a placeholder immediately so any concurrent caller returns early.
        {
            use dashmap::mapref::entry::Entry;
            match self.active.entry(task_id.clone()) {
                Entry::Occupied(occ) => {
                    let active = occ.get();
                    if active.run_generation == run_generation
                        && active.owner_session == owner_session
                    {
                        return Ok(WorkspaceLease {
                            workspace_path: active.workspace_path.clone(),
                            owner_session,
                            run_generation,
                            decision: WorkspaceAcquireDecision::ReusedTracked,
                        });
                    }
                    return Err(WorkspaceLifecycleError::LiveForeignOwner {
                        workspace_path: active.workspace_path.clone(),
                        workspace_owner: Some(active.owner_session.clone()),
                        message: format!(
                            "WorktreeCollision: workspace path {:?} already owned by another harness session; manual resolution required",
                            active.workspace_path
                        ),
                    });
                }
                Entry::Vacant(vac) => {
                    vac.insert(ActiveWorkspace {
                        workspace_path: workspace_path.clone(),
                        source_repo: source_repo.to_path_buf(),
                        owner_session: owner_session.clone(),
                        run_generation,
                    });
                }
            }
        }

        let mut decision = WorkspaceAcquireDecision::CreatedFresh;

        if workspace_path.exists() {
            let owner_record = read_owner_record(&workspace_path);
            if owner_record.as_ref().is_some_and(|record| {
                record.task_id == task_id.0
                    && record.run_generation == run_generation
                    && record.owner_session != owner_session
            }) {
                self.active.remove(task_id);
                return Err(WorkspaceLifecycleError::LiveForeignOwner {
                    workspace_path: workspace_path.clone(),
                    workspace_owner: owner_record.map(|record| record.owner_session),
                    message: format!(
                        "WorktreeCollision: workspace path {:?} is managed by another harness session for task {} generation {}; manual resolution required",
                        workspace_path, task_id.0, run_generation
                    ),
                });
            }

            if owner_record.as_ref().is_some_and(|record| {
                record.task_id == task_id.0
                    && record.run_generation == run_generation
                    && record.owner_session == owner_session
            }) {
                tracing::info!(
                    task_id = %task_id.0,
                    path = ?workspace_path,
                    run_generation,
                    "workspace recovery: re-using recorded worktree from previous run"
                );
                return Ok(WorkspaceLease {
                    workspace_path,
                    owner_session,
                    run_generation,
                    decision: WorkspaceAcquireDecision::ReusedRecovered,
                });
            }

            if let Err(err) = cleanup_workspace_path(source_repo, &workspace_path).await {
                self.active.remove(task_id);
                return Err(WorkspaceLifecycleError::ReconcileFailed {
                    workspace_path: workspace_path.clone(),
                    workspace_owner: owner_record.map(|record| record.owner_session),
                    message: format!(
                        "workspace lifecycle reconciliation failed for task {} at {:?}: {err}",
                        task_id.0, workspace_path
                    ),
                });
            }
            decision = WorkspaceAcquireDecision::RecreatedStale;
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
            .await
            .map_err(|e| WorkspaceLifecycleError::CreateFailed {
                workspace_path: workspace_path.clone(),
                workspace_owner: Some(owner_session.clone()),
                message: format!("git fetch failed for task {}: {e}", task_id.0),
            })?;

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
            .await
            .map_err(|e| WorkspaceLifecycleError::CreateFailed {
                workspace_path: workspace_path.clone(),
                workspace_owner: Some(owner_session.clone()),
                message: format!("git worktree add failed for task {}: {e}", task_id.0),
            })?;

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
                .await
                .map_err(|e| WorkspaceLifecycleError::CreateFailed {
                    workspace_path: workspace_path.clone(),
                    workspace_owner: Some(owner_session.clone()),
                    message: format!(
                        "git worktree add fallback failed for task {}: {e}",
                        task_id.0
                    ),
                })?;

            if !fallback.status.success() {
                self.active.remove(task_id);
                let stderr = String::from_utf8_lossy(&fallback.stderr);
                return Err(WorkspaceLifecycleError::CreateFailed {
                    workspace_path: workspace_path.clone(),
                    workspace_owner: Some(owner_session.clone()),
                    message: format!(
                        "git worktree add failed for task {}: {}",
                        task_id.0,
                        stderr.trim()
                    ),
                });
            }
        }

        let owner_record = WorkspaceOwnerRecord {
            task_id: task_id.0.clone(),
            run_generation,
            owner_session: owner_session.clone(),
        };
        if let Err(err) = write_owner_record(&workspace_path, &owner_record) {
            self.active.remove(task_id);
            if let Err(cleanup_err) = cleanup_workspace_path(source_repo, &workspace_path).await {
                tracing::warn!(
                    "failed to cleanup partial worktree after owner-record failure: {cleanup_err}"
                );
            }
            return Err(WorkspaceLifecycleError::CreateFailed {
                workspace_path: workspace_path.clone(),
                workspace_owner: Some(owner_session.clone()),
                message: format!("failed to persist workspace owner record: {err}"),
            });
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
                if let Err(cleanup_err) = cleanup_workspace_path(source_repo, &workspace_path).await
                {
                    tracing::warn!(
                        "failed to cleanup partial worktree after hook failure: {cleanup_err}"
                    );
                }
                return Err(WorkspaceLifecycleError::CreateFailed {
                    workspace_path,
                    workspace_owner: Some(owner_session),
                    message: err_msg,
                });
            }
        }

        Ok(WorkspaceLease {
            workspace_path,
            owner_session,
            run_generation,
            decision,
        })
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

        if let Err(e) = cleanup_workspace_path(&entry.source_repo, &entry.workspace_path).await {
            tracing::warn!(
                "git worktree remove failed for {:?}: {e}",
                entry.workspace_path
            );
        }
        Ok(())
    }

    pub async fn cleanup_workspace_for_retry(
        &self,
        task_id: &TaskId,
        source_repo: &Path,
        workspace_path: Option<&Path>,
    ) -> anyhow::Result<()> {
        self.active.remove(task_id);
        let fallback_path = self.config.root.join(sanitize_task_id(&task_id.0));
        let target = workspace_path
            .map(Path::to_path_buf)
            .unwrap_or(fallback_path);
        cleanup_workspace_path(source_repo, &target).await
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

    pub(crate) async fn reconcile_startup(
        &self,
        source_repo: &Path,
        tasks: &[TaskSummary],
    ) -> anyhow::Result<StartupReconciliation> {
        let task_by_id: std::collections::HashMap<&str, &TaskSummary> = tasks
            .iter()
            .map(|task| (task.id.0.as_str(), task))
            .collect();
        let path_to_task: std::collections::HashMap<PathBuf, &TaskSummary> = tasks
            .iter()
            .map(|task| {
                let path = task
                    .workspace_path
                    .as_ref()
                    .map(PathBuf::from)
                    .unwrap_or_else(|| self.config.root.join(sanitize_task_id(&task.id.0)));
                (path, task)
            })
            .collect();

        let read_dir = std::fs::read_dir(&self.config.root)?;
        let mut summary = StartupReconciliation::default();
        let mut seen_paths = std::collections::HashSet::new();

        for entry in read_dir.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            seen_paths.insert(path.clone());
            if self.active.iter().any(|e| e.workspace_path == path) {
                continue;
            }

            let owner_record = read_owner_record(&path);
            let task = owner_record
                .as_ref()
                .and_then(|record| task_by_id.get(record.task_id.as_str()).copied())
                .or_else(|| path_to_task.get(&path).copied());

            let should_remove = match task {
                None => true,
                Some(task) if task.status.is_terminal() => true,
                Some(task) => {
                    if let Some(owner_record) = owner_record.as_ref() {
                        if owner_record.run_generation != task.run_generation {
                            true
                        } else if let Some(expected_owner) = task.workspace_owner.as_deref() {
                            expected_owner == self.owner_session
                                && owner_record.owner_session == expected_owner
                        } else {
                            false
                        }
                    } else {
                        false
                    }
                }
            };

            if should_remove {
                let cleanup_repo = resolve_cleanup_source_repo(source_repo, &path, task).await;
                match cleanup_workspace_path(&cleanup_repo, &path).await {
                    Ok(()) => {
                        summary.removed += 1;
                    }
                    Err(e) => {
                        tracing::warn!(
                            path = ?path,
                            source_repo = ?cleanup_repo,
                            "reconcile_startup: failed to cleanup workspace: {e}"
                        );
                    }
                }
            } else {
                summary.preserved += 1;
            }
        }

        for (path, task) in &path_to_task {
            if seen_paths.contains(path)
                || self.active.iter().any(|e| e.workspace_path == *path)
                || path.exists()
            {
                continue;
            }

            let cleanup_repo = resolve_cleanup_source_repo(source_repo, path, Some(*task)).await;
            if !is_registered_worktree(&cleanup_repo, path).await {
                continue;
            }

            match cleanup_workspace_path(&cleanup_repo, path).await {
                Ok(()) => {
                    summary.removed += 1;
                }
                Err(e) => {
                    tracing::warn!(
                        path = ?path,
                        source_repo = ?cleanup_repo,
                        "reconcile_startup: failed to cleanup missing workspace registration: {e}"
                    );
                }
            }
        }

        Ok(summary)
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

fn workspace_git_dir(workspace_path: &Path) -> anyhow::Result<PathBuf> {
    let dot_git = workspace_path.join(".git");
    let metadata = std::fs::metadata(&dot_git)?;
    if metadata.is_dir() {
        return Ok(dot_git);
    }

    let gitdir = std::fs::read_to_string(&dot_git)?;
    let relative = gitdir
        .trim()
        .strip_prefix("gitdir:")
        .map(str::trim)
        .ok_or_else(|| anyhow::anyhow!("invalid gitdir metadata at {:?}", dot_git))?;
    let gitdir_path = Path::new(relative);
    Ok(if gitdir_path.is_absolute() {
        gitdir_path.to_path_buf()
    } else {
        workspace_path.join(gitdir_path)
    })
}

fn owner_record_path(workspace_path: &Path) -> anyhow::Result<PathBuf> {
    Ok(workspace_git_dir(workspace_path)?.join(OWNER_RECORD_FILE))
}

fn read_owner_record(workspace_path: &Path) -> Option<WorkspaceOwnerRecord> {
    let bytes = std::fs::read(owner_record_path(workspace_path).ok()?).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn write_owner_record(
    workspace_path: &Path,
    owner_record: &WorkspaceOwnerRecord,
) -> anyhow::Result<()> {
    let bytes = serde_json::to_vec(owner_record)?;
    std::fs::write(owner_record_path(workspace_path)?, bytes)?;
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

async fn cleanup_workspace_path(source_repo: &Path, workspace_path: &Path) -> anyhow::Result<()> {
    let worktree_registered = is_registered_worktree(source_repo, workspace_path).await;
    if worktree_registered {
        match remove_worktree(source_repo, workspace_path).await {
            Ok(()) => {}
            Err(e) if !workspace_path.exists() => {
                tracing::warn!(
                    path = ?workspace_path,
                    "cleanup_workspace_path: git worktree remove failed for missing path; pruning stale metadata: {e}"
                );
            }
            Err(e) => {
                tracing::warn!(path = ?workspace_path, "cleanup_workspace_path: git worktree remove failed for existing path: {e}");
            }
        }
    }

    if workspace_path.exists() {
        std::fs::remove_dir_all(workspace_path)?;
    }

    if let Err(e) = git_command()
        .args(["-C", &source_repo.to_string_lossy(), "worktree", "prune"])
        .output()
        .await
    {
        tracing::warn!("cleanup_workspace_path: git worktree prune failed: {e}");
    }

    Ok(())
}

async fn resolve_cleanup_source_repo(
    default_source_repo: &Path,
    workspace_path: &Path,
    task: Option<&TaskSummary>,
) -> PathBuf {
    if let Some(project_root) = task.and_then(|task| task.project.as_deref()) {
        return PathBuf::from(project_root);
    }

    infer_workspace_source_repo(workspace_path)
        .await
        .unwrap_or_else(|| default_source_repo.to_path_buf())
}

async fn infer_workspace_source_repo(workspace_path: &Path) -> Option<PathBuf> {
    git_command()
        .args([
            "-C",
            &workspace_path.to_string_lossy(),
            "rev-parse",
            "--show-toplevel",
        ])
        .output()
        .await
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|stdout| PathBuf::from(stdout.trim()))
}

async fn is_registered_worktree(source_repo: &Path, workspace_path: &Path) -> bool {
    // `git worktree list --porcelain` emits absolute paths even when `workspace.root`
    // was configured relatively. Deleted worktrees may still be listed through a
    // symlink-expanded parent such as `/private/var`, so normalize through the
    // nearest existing ancestor before matching.
    let expected_path = canonicalize_existing_or_parent(workspace_path);
    git_command()
        .args([
            "-C",
            &source_repo.to_string_lossy(),
            "worktree",
            "list",
            "--porcelain",
        ])
        .output()
        .await
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|stdout| {
            stdout.lines().any(|line| {
                line.strip_prefix("worktree ")
                    .map(PathBuf::from)
                    .map(|listed| canonicalize_existing_or_parent(&listed))
                    .is_some_and(|listed| listed == expected_path)
            })
        })
        .unwrap_or(false)
}

fn canonicalize_existing_or_parent(path: &Path) -> PathBuf {
    if let Ok(canonical) = std::fs::canonicalize(path) {
        return canonical;
    }

    let mut missing_components = Vec::new();
    let mut cursor = path;
    while !cursor.exists() {
        let Some(parent) = cursor.parent() else {
            return path.to_path_buf();
        };
        let Some(file_name) = cursor.file_name() else {
            return path.to_path_buf();
        };
        missing_components.push(file_name.to_os_string());
        cursor = parent;
    }

    let mut normalized = std::fs::canonicalize(cursor).unwrap_or_else(|_| cursor.to_path_buf());
    for component in missing_components.iter().rev() {
        normalized.push(component);
    }
    normalized
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

    fn async_env_lock() -> &'static tokio::sync::Mutex<()> {
        static LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
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
            .create_workspace(&task_id, source.path(), "origin", &branch, 1)
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
            .create_workspace(&task_id, source.path(), "origin", &branch, 1)
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
            .create_workspace(&task_id, source.path(), "origin", &branch, 1)
            .await
            .expect("create first");
        let path2 = mgr
            .create_workspace(&task_id, source.path(), "origin", &branch, 1)
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
            .create_workspace(&task_id, source.path(), "origin", &branch, 1)
            .await
            .expect("create");
        assert!(ws_path.workspace_path.is_dir());

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

        mgr.create_workspace(&task_id, source.path(), "origin", &branch, 1)
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
            .create_workspace(&task_id, source.path(), "origin", &branch, 1)
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
        let stale_path = workspaces.path().join("stale-task-check-001");
        std::fs::create_dir_all(&stale_path).expect("create stale dir");

        let result = mgr
            .create_workspace(&task_id, source.path(), "origin", &branch, 1)
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
            .create_workspace(&task_id, source.path(), "origin", &branch, 1)
            .await
            .expect("create first owner");

        let err = mgr_b
            .create_workspace(&task_id, source.path(), "origin", &branch, 1)
            .await
            .expect_err("second owner should be blocked");
        assert!(
            err.to_string().contains("manual resolution required"),
            "foreign live owner should remain a protected hard stop: {err}"
        );
    }

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
            .create_workspace(&task_id, source.path(), "origin", &branch, 1)
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
            .create_workspace(&task_id, source_a.path(), "origin", &branch_a, 1)
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
            .create_workspace(&task_id, source.path(), "origin", &branch, 1)
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
            .create_workspace(&task_id, source.path(), "origin", &branch, 1)
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
            .create_workspace(&task_id, source.path(), "origin", &branch, 2)
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

    struct CwdGuard(PathBuf);

    impl CwdGuard {
        fn switch_to(path: &Path) -> anyhow::Result<Self> {
            let original = std::env::current_dir()?;
            std::env::set_current_dir(path)?;
            Ok(Self(original))
        }
    }

    impl Drop for CwdGuard {
        fn drop(&mut self) {
            let _ = std::env::set_current_dir(&self.0);
        }
    }

    #[tokio::test]
    async fn is_registered_worktree_matches_relative_workspace_paths() {
        let _guard = async_env_lock().lock().await;

        let sandbox = tempfile::tempdir().expect("tempdir");
        let _cwd_guard = CwdGuard::switch_to(sandbox.path()).expect("switch cwd");

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

        mgr.create_workspace(&task_id, &source, "origin", &branch, 1)
            .await
            .expect("create");
        let relative_workspace_path =
            PathBuf::from("workspaces").join(sanitize_task_id(&task_id.0));

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
            mgr.create_workspace(id, source.path(), "origin", &branch, 1)
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
