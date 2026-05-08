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

fn git_binary() -> String {
    std::env::var("HARNESS_GIT_BIN").unwrap_or_else(|_| "git".to_string())
}

fn git_command() -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new(git_binary());
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WorkspaceCreateOptions {
    pub(crate) require_remote_head: bool,
    pub(crate) reuse_existing_workspace: bool,
    pub(crate) after_create_hook: Option<String>,
    pub(crate) hook_timeout_secs: Option<u64>,
    pub(crate) branch_prefix: String,
}

impl Default for WorkspaceCreateOptions {
    fn default() -> Self {
        Self {
            require_remote_head: true,
            reuse_existing_workspace: true,
            after_create_hook: None,
            hook_timeout_secs: None,
            branch_prefix: "harness/".to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct WorkspaceOwnerRecord {
    task_id: String,
    run_generation: u32,
    owner_session: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    workspace_key: Option<String>,
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
    /// Dirs whose owner record shows a new-key (issue/PR) task that was terminal.
    pub(crate) migrated: u32,
}

/// Summary produced by the periodic disk reconciliation scan.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DiskReconciliationSummary {
    pub(crate) scanned: u32,
    pub(crate) removed: u32,
    pub(crate) skipped_uuid: u32,
    pub(crate) skipped_open: u32,
}

/// Minimal rate-limiter for the disk reconciliation scan's GitHub API calls.
struct DiskRateLimiter {
    max_per_minute: u32,
    calls_this_window: u32,
    window_start: std::time::Instant,
}

impl DiskRateLimiter {
    fn new(max_per_minute: u32) -> Self {
        Self {
            max_per_minute,
            calls_this_window: 0,
            window_start: std::time::Instant::now(),
        }
    }

    async fn acquire(&mut self) {
        if self.max_per_minute == 0 {
            return;
        }
        if self.window_start.elapsed() >= std::time::Duration::from_secs(60) {
            self.window_start = std::time::Instant::now();
            self.calls_this_window = 0;
        }
        if self.calls_this_window >= self.max_per_minute {
            let remaining =
                std::time::Duration::from_secs(60).saturating_sub(self.window_start.elapsed());
            if !remaining.is_zero() {
                tokio::time::sleep(remaining).await;
            }
            self.window_start = std::time::Instant::now();
            self.calls_this_window = 0;
        }
        self.calls_this_window += 1;
    }
}

pub struct WorkspaceManager {
    pub(crate) config: WorkspaceConfig,
    pub(crate) active: DashMap<TaskId, ActiveWorkspace>,
    pub(crate) active_paths: DashMap<PathBuf, TaskId>,
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
            active_paths: DashMap::new(),
            owner_session: SessionId::new().to_string(),
        })
    }

    fn release_active_path(&self, task_id: &TaskId, workspace_path: &Path) {
        let owned_by_task = self
            .active_paths
            .get(workspace_path)
            .is_some_and(|owner| owner.value() == task_id);
        if owned_by_task {
            self.active_paths.remove(workspace_path);
        }
    }

    fn remove_active_workspace(&self, task_id: &TaskId) -> Option<ActiveWorkspace> {
        let (_, active) = self.active.remove(task_id)?;
        self.release_active_path(task_id, &active.workspace_path);
        Some(active)
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
        external_id: Option<&str>,
        repo: Option<&str>,
    ) -> Result<WorkspaceLease, WorkspaceLifecycleError> {
        self.create_workspace_with_options(
            task_id,
            source_repo,
            remote,
            base_branch,
            run_generation,
            external_id,
            repo,
            WorkspaceCreateOptions::default(),
        )
        .await
    }

    pub(crate) async fn create_workspace_with_options(
        &self,
        task_id: &TaskId,
        source_repo: &Path,
        remote: &str,
        base_branch: &str,
        run_generation: u32,
        external_id: Option<&str>,
        repo: Option<&str>,
        options: WorkspaceCreateOptions,
    ) -> Result<WorkspaceLease, WorkspaceLifecycleError> {
        let workspace_key = derive_workspace_key(task_id, external_id, repo, Some(source_repo));
        // Validate inputs to prevent unexpected git behavior.
        if !is_valid_branch_name(base_branch) {
            return Err(WorkspaceLifecycleError::CreateFailed {
                workspace_path: self.config.root.join(&workspace_key),
                workspace_owner: None,
                message: format!("invalid base_branch: {base_branch:?}"),
            });
        }
        if !is_valid_branch_name(remote) {
            return Err(WorkspaceLifecycleError::CreateFailed {
                workspace_path: self.config.root.join(&workspace_key),
                workspace_owner: None,
                message: format!("invalid remote: {remote:?}"),
            });
        }

        let workspace_path = self.config.root.join(&workspace_key);
        let owner_session = self.owner_session.clone();

        // Reserve the deterministic path before task insertion. This closes the
        // race where two different task IDs derive the same issue/PR workspace.
        let path_reservation_inserted = {
            use dashmap::mapref::entry::Entry;
            match self.active_paths.entry(workspace_path.clone()) {
                Entry::Occupied(occ) => {
                    let owner_task = occ.get().clone();
                    if owner_task != task_id.clone() {
                        let (existing_path, workspace_owner) = self
                            .active
                            .get(&owner_task)
                            .map(|entry| {
                                (
                                    entry.workspace_path.clone(),
                                    Some(entry.owner_session.clone()),
                                )
                            })
                            .unwrap_or_else(|| (workspace_path.clone(), None));
                        return Err(WorkspaceLifecycleError::LiveForeignOwner {
                            workspace_path: existing_path.clone(),
                            workspace_owner,
                            message: format!(
                                "WorktreeCollision: workspace path {:?} already reserved by active task {}; manual resolution required",
                                existing_path,
                                owner_task.0
                            ),
                        });
                    }
                    false
                }
                Entry::Vacant(vac) => {
                    vac.insert(task_id.clone());
                    true
                }
            }
        };

        // Atomic task check-and-insert: prevents TOCTOU where two concurrent calls
        // for the same task_id would both attempt git worktree add on the same path.
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
                    if path_reservation_inserted {
                        self.release_active_path(task_id, &workspace_path);
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
                owner_record_matches_workspace(record, task_id, &workspace_key, run_generation)
                    && record.owner_session != owner_session
            }) {
                self.remove_active_workspace(task_id);
                return Err(WorkspaceLifecycleError::LiveForeignOwner {
                    workspace_path: workspace_path.clone(),
                    workspace_owner: owner_record.map(|record| record.owner_session),
                    message: format!(
                        "WorktreeCollision: workspace path {:?} is managed by another harness session for task {} generation {}; manual resolution required",
                        workspace_path, task_id.0, run_generation
                    ),
                });
            }

            if options.reuse_existing_workspace
                && owner_record.as_ref().is_some_and(|record| {
                    owner_record_matches_workspace(record, task_id, &workspace_key, run_generation)
                        && record.owner_session == owner_session
                })
            {
                let owner_record = WorkspaceOwnerRecord {
                    task_id: task_id.0.clone(),
                    run_generation,
                    owner_session: owner_session.clone(),
                    workspace_key: Some(workspace_key.clone()),
                };
                if let Err(err) = write_owner_record(&workspace_path, &owner_record) {
                    self.remove_active_workspace(task_id);
                    return Err(WorkspaceLifecycleError::CreateFailed {
                        workspace_path: workspace_path.clone(),
                        workspace_owner: Some(owner_session.clone()),
                        message: format!("failed to update workspace owner record: {err}"),
                    });
                }
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
                self.remove_active_workspace(task_id);
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
        let fetch_output = match git_command()
            .args([
                "-C",
                &source_repo.to_string_lossy(),
                "fetch",
                remote,
                base_branch,
            ])
            .output()
            .await
        {
            Ok(output) => output,
            Err(e) => {
                self.remove_active_workspace(task_id);
                return Err(WorkspaceLifecycleError::CreateFailed {
                    workspace_path: workspace_path.clone(),
                    workspace_owner: Some(owner_session.clone()),
                    message: format!("git fetch failed for task {}: {e}", task_id.0),
                });
            }
        };

        if !fetch_output.status.success() {
            let stderr = String::from_utf8_lossy(&fetch_output.stderr);
            if options.require_remote_head {
                self.remove_active_workspace(task_id);
                return Err(WorkspaceLifecycleError::CreateFailed {
                    workspace_path: workspace_path.clone(),
                    workspace_owner: Some(owner_session.clone()),
                    message: format!(
                        "git fetch {remote} {base_branch} failed for task {}: {}",
                        task_id.0,
                        stderr.trim()
                    ),
                });
            } else {
                tracing::warn!(
                    "git fetch {remote} {base_branch} failed (continuing with local): {}",
                    stderr.trim()
                );
            }
        }

        // Create git worktree based on remote/base_branch (latest upstream).
        // Falls back to local base_branch only when strict remote-head
        // admission is disabled.
        let remote_ref = format!("{remote}/{base_branch}");
        let branch = format!("{}{}", options.branch_prefix, task_id.0);
        if !is_valid_branch_name(&branch) {
            self.remove_active_workspace(task_id);
            return Err(WorkspaceLifecycleError::CreateFailed {
                workspace_path: workspace_path.clone(),
                workspace_owner: Some(owner_session.clone()),
                message: format!("invalid workspace branch: {branch:?}"),
            });
        }
        let output = match git_command()
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
        {
            Ok(output) => output,
            Err(e) => {
                self.remove_active_workspace(task_id);
                return Err(WorkspaceLifecycleError::CreateFailed {
                    workspace_path: workspace_path.clone(),
                    workspace_owner: Some(owner_session.clone()),
                    message: format!("git worktree add failed for task {}: {e}", task_id.0),
                });
            }
        };

        if !output.status.success() {
            if options.require_remote_head {
                self.remove_active_workspace(task_id);
                let stderr = String::from_utf8_lossy(&output.stderr);
                return Err(WorkspaceLifecycleError::CreateFailed {
                    workspace_path: workspace_path.clone(),
                    workspace_owner: Some(owner_session.clone()),
                    message: format!(
                        "git worktree add from {remote_ref} failed for task {}: {}",
                        task_id.0,
                        stderr.trim()
                    ),
                });
            }
            // Fallback: try local base_branch (useful for repos without remotes, e.g. tests).
            let fallback = match git_command()
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
            {
                Ok(output) => output,
                Err(e) => {
                    self.remove_active_workspace(task_id);
                    return Err(WorkspaceLifecycleError::CreateFailed {
                        workspace_path: workspace_path.clone(),
                        workspace_owner: Some(owner_session.clone()),
                        message: format!(
                            "git worktree add fallback failed for task {}: {e}",
                            task_id.0
                        ),
                    });
                }
            };

            if !fallback.status.success() {
                self.remove_active_workspace(task_id);
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
            workspace_key: Some(workspace_key),
        };
        if let Err(err) = write_owner_record(&workspace_path, &owner_record) {
            self.remove_active_workspace(task_id);
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
        let after_create_hook = options
            .after_create_hook
            .as_deref()
            .or(self.config.after_create_hook.as_deref());
        if let Some(hook) = after_create_hook {
            let timeout_secs = options
                .hook_timeout_secs
                .unwrap_or(self.config.hook_timeout_secs);
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
                self.remove_active_workspace(task_id);
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
        let entry = match self.remove_active_workspace(task_id) {
            Some(entry) => entry,
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

    /// Release the in-memory lease without deleting the workspace on disk.
    ///
    /// Used when `auto_cleanup=false` so a later task with the same deterministic
    /// issue/PR workspace key can reuse the directory while concurrent tasks are
    /// still protected by the active-path collision check.
    pub fn release_workspace(&self, task_id: &TaskId) {
        self.remove_active_workspace(task_id);
    }

    pub async fn cleanup_workspace_for_retry(
        &self,
        task_id: &TaskId,
        source_repo: &Path,
        workspace_path: Option<&Path>,
    ) -> anyhow::Result<()> {
        // Resolve target before removing from active so deterministic-key workspaces
        // (whose directory name differs from sanitize_task_id(task_id)) are found.
        let target = workspace_path
            .map(Path::to_path_buf)
            .or_else(|| self.active.get(task_id).map(|e| e.workspace_path.clone()))
            .unwrap_or_else(|| self.config.root.join(sanitize_task_id(&task_id.0)));
        if let Some(owner_task) = self.active_paths.get(&target) {
            if owner_task.value() != task_id {
                tracing::warn!(
                    task_id = %task_id.0,
                    owner_task_id = %owner_task.value().0,
                    workspace_path = ?target,
                    "cleanup_workspace_for_retry: skipped deleting workspace reserved by another active task"
                );
                return Ok(());
            }
        }
        self.remove_active_workspace(task_id);
        if let Some(owner_task) = self.active_paths.get(&target) {
            tracing::warn!(
                task_id = %task_id.0,
                owner_task_id = %owner_task.value().0,
                workspace_path = ?target,
                "cleanup_workspace_for_retry: skipped deleting workspace claimed during retry cleanup"
            );
            return Ok(());
        }
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
        let mut path_to_tasks: std::collections::HashMap<PathBuf, Vec<&TaskSummary>> =
            std::collections::HashMap::new();
        for task in tasks {
            let path = task_summary_workspace_path(&self.config.root, task);
            path_to_tasks.entry(path).or_default().push(task);
        }

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
            let mut candidate_tasks = Vec::new();
            if let Some(task) = owner_record
                .as_ref()
                .and_then(|record| task_by_id.get(record.task_id.as_str()).copied())
            {
                candidate_tasks.push(task);
            }
            if let Some(path_tasks) = path_to_tasks.get(&path) {
                for path_task in path_tasks {
                    if !candidate_tasks
                        .iter()
                        .any(|candidate: &&TaskSummary| candidate.id == path_task.id)
                    {
                        candidate_tasks.push(*path_task);
                    }
                }
            }

            let should_remove = match candidate_tasks.as_slice() {
                [] => true,
                [task] if task.status.is_terminal() => true,
                [task] => {
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
                tasks => !tasks.iter().any(|task| !task.status.is_terminal()),
            };

            if should_remove {
                let cleanup_repo = resolve_cleanup_source_repo(
                    source_repo,
                    &path,
                    candidate_tasks.first().copied(),
                )
                .await;
                // Classify as migrated when the owner record carries an issue/PR key
                // (new-key workspace from slice 1), so startup logs show the right breakdown.
                let is_new_key = owner_record
                    .as_ref()
                    .and_then(owner_record_external_id)
                    .is_some();
                match cleanup_workspace_path(&cleanup_repo, &path).await {
                    Ok(()) => {
                        if is_new_key {
                            summary.migrated += 1;
                        } else {
                            summary.removed += 1;
                        }
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

        for (path, path_tasks) in &path_to_tasks {
            if seen_paths.contains(path)
                || self.active.iter().any(|e| e.workspace_path == *path)
                || path.exists()
            {
                continue;
            }

            let cleanup_repo =
                resolve_cleanup_source_repo(source_repo, path, path_tasks.first().copied()).await;
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

    /// Walk `config.root` and remove any workspace directory whose owner record identifies
    /// a closed issue or merged/closed PR, querying GitHub state through the REST API.
    ///
    /// UUID-keyed dirs (no parseable external_id in the owner record) are skipped — they
    /// are handled by the spawn.rs GC-on-task-done path. Errors from individual removals
    /// are logged and do not abort the sweep.
    pub(crate) async fn reconcile_disk_workspaces(
        &self,
        source_repo: &Path,
        _gh_bin: &str,
        max_rate: u32,
        github_token: Option<&str>,
    ) -> DiskReconciliationSummary {
        let mut summary = DiskReconciliationSummary::default();
        let read_dir = match std::fs::read_dir(&self.config.root) {
            Ok(rd) => rd,
            Err(e) => {
                tracing::warn!(
                    "reconcile_disk_workspaces: failed to read {:?}: {e}",
                    self.config.root
                );
                return summary;
            }
        };

        let mut rate = DiskRateLimiter::new(max_rate);

        for entry in read_dir.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            summary.scanned += 1;

            // Active workspaces are owned by a running task — do not touch them.
            if self.active.iter().any(|e| e.workspace_path == path) {
                summary.skipped_open += 1;
                continue;
            }

            let owner_record = read_owner_record(&path);
            let Some(owner_record) = owner_record.as_ref() else {
                summary.skipped_uuid += 1;
                continue;
            };
            let Some(external_id) = owner_record_external_id(owner_record) else {
                summary.skipped_uuid += 1;
                continue;
            };
            let (issue_num, pr_num) = crate::reconciliation::parse_external_id(Some(&external_id));
            if issue_num.is_none() && pr_num.is_none() {
                summary.skipped_uuid += 1;
                continue;
            }
            let Some(repo_slug) = owner_record
                .workspace_key
                .as_deref()
                .and_then(repo_slug_from_workspace_key)
            else {
                summary.skipped_open += 1;
                continue;
            };

            let gh_state = if let Some(n) = issue_num {
                rate.acquire().await;
                crate::reconciliation::fetch_issue_state_with_token(&repo_slug, n, github_token)
                    .await
            } else if let Some(n) = pr_num {
                rate.acquire().await;
                crate::reconciliation::fetch_pr_state_by_slug_with_token(
                    &repo_slug,
                    n,
                    github_token,
                )
                .await
            } else {
                crate::reconciliation::GitHubState::Unknown
            };

            let should_remove = matches!(
                gh_state,
                crate::reconciliation::GitHubState::IssueClosed
                    | crate::reconciliation::GitHubState::PrMerged
                    | crate::reconciliation::GitHubState::PrClosed
            );

            if should_remove {
                match cleanup_workspace_path(source_repo, &path).await {
                    Ok(()) => summary.removed += 1,
                    Err(e) => tracing::warn!(
                        "reconcile_disk_workspaces: cleanup failed for {path:?}: {e}"
                    ),
                }
            } else {
                summary.skipped_open += 1;
            }
        }

        tracing::info!(
            scanned = summary.scanned,
            removed = summary.removed,
            skipped_uuid = summary.skipped_uuid,
            skipped_open = summary.skipped_open,
            "reconcile_disk_workspaces: scan complete"
        );
        summary
    }

    /// Scan `config.root` for worktree directories and remove any that correspond to
    /// terminal (Done/Failed) task IDs and are not currently tracked as active.
    /// This cleans up orphaned worktrees left behind by a previous server crash.
    ///
    /// Errors from individual removals are logged and do not abort the sweep.
    pub async fn cleanup_orphan_worktrees(&self, source_repo: &Path, terminal_task_ids: &[TaskId]) {
        let terminal_set: std::collections::HashSet<&str> =
            terminal_task_ids.iter().map(|id| id.0.as_str()).collect();
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
            //
            // Deterministic workspace keys (e.g. `{hash}__{repo}__{issue}`) don't match
            // the UUID-derived directory name pattern, so also check the owner record.
            let is_terminal = terminal_dirs.iter().any(|td| {
                dir_name == *td
                    || dir_name == format!("{td}-seq")
                    || dir_name
                        .strip_prefix(&format!("{td}-p"))
                        .is_some_and(|rest| {
                            !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit())
                        })
            }) || read_owner_record(&path)
                .map(|record| terminal_set.contains(record.task_id.as_str()))
                .unwrap_or(false);
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

/// Sanitize a GitHub repository slug for use as a filesystem path component.
///
/// Preserves underscores, dots, and hyphens (all valid in repo names) so that
/// `my.org/repo` and `my_org/repo` produce distinct keys (`my.org_repo` vs
/// `my_org_repo`). The `/` org-repo separator maps to `_`. GitHub organisation
/// names cannot contain underscores (only `[a-zA-Z0-9-]`), so the `owner_repo`
/// output is unambiguous for valid GitHub slugs.
fn sanitize_repo_slug(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '.' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Return an 8-character lowercase hex string from a 32-bit FNV-1a hash of `s`.
///
/// This is a deterministic, stable hash with no external dependencies, used to
/// produce a unique project scope component in deterministic workspace keys.
fn fnv1a_8(s: &str) -> String {
    let mut hash: u32 = 0x811c9dc5;
    for b in s.bytes() {
        hash ^= u32::from(b);
        hash = hash.wrapping_mul(0x01000193);
    }
    format!("{hash:08x}")
}

/// Derive the filesystem key for a workspace.
///
/// For tasks with `external_id` matching `issue:N` or `pr:N` and a non-empty `repo`,
/// returns `<path_hash>__<sanitized_repo>__<sanitized_external_id>`
/// (e.g. `a3f2b1c4__myorg_my-repo__issue_42`), scoped by a hash of the project's
/// absolute path so that two different projects targeting the same GitHub repo/issue
/// do not collide even when their directory names are identical.
/// Falls back to the UUID-derived key when `external_id`/`repo` are absent or don't match.
fn derive_workspace_key(
    task_id: &TaskId,
    external_id: Option<&str>,
    repo: Option<&str>,
    source_repo: Option<&std::path::Path>,
) -> String {
    if let (Some(eid), Some(r)) = (external_id, repo) {
        if !r.is_empty() && is_issue_or_pr_id(eid) {
            let project_prefix = source_repo
                .map(|p| {
                    let canonical = p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
                    format!("{}__", fnv1a_8(&canonical.to_string_lossy()))
                })
                .unwrap_or_default();
            return format!(
                "{}{}__{}",
                project_prefix,
                sanitize_repo_slug(r),
                sanitize_task_id(eid)
            );
        }
    }
    sanitize_task_id(&task_id.0)
}

fn is_issue_or_pr_id(s: &str) -> bool {
    let digits = if let Some(rest) = s.strip_prefix("issue:") {
        rest
    } else if let Some(rest) = s.strip_prefix("pr:") {
        rest
    } else {
        return false;
    };
    !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit())
}

fn owner_record_external_id(record: &WorkspaceOwnerRecord) -> Option<String> {
    let (issue, pr) = crate::reconciliation::parse_external_id(Some(&record.task_id));
    if issue.is_some() || pr.is_some() {
        return Some(record.task_id.clone());
    }
    record
        .workspace_key
        .as_deref()
        .and_then(external_id_from_workspace_key)
}

fn external_id_from_workspace_key(key: &str) -> Option<String> {
    let suffix = key.rsplit("__").next()?;
    if let Some(issue) = suffix.strip_prefix("issue_") {
        if !issue.is_empty() && issue.chars().all(|c| c.is_ascii_digit()) {
            return Some(format!("issue:{issue}"));
        }
    }
    if let Some(pr) = suffix.strip_prefix("pr_") {
        if !pr.is_empty() && pr.chars().all(|c| c.is_ascii_digit()) {
            return Some(format!("pr:{pr}"));
        }
    }
    None
}

fn repo_slug_from_workspace_key(key: &str) -> Option<String> {
    let mut parts = key.rsplit("__");
    let _external_id = parts.next()?;
    let repo_part = parts.next()?;
    let (owner, repo) = repo_part.split_once('_')?;
    if owner.is_empty() || repo.is_empty() {
        return None;
    }
    Some(format!("{owner}/{repo}"))
}

/// Returns true when the git worktree at `path` is currently on `branch`.
/// Used to distinguish crash-recovery (same task's worktree) from a true collision.
pub(crate) async fn run_hook(script: &str, cwd: &Path) -> anyhow::Result<()> {
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

fn task_summary_workspace_path(root: &Path, task: &TaskSummary) -> PathBuf {
    task.workspace_path
        .as_ref()
        .map(PathBuf::from)
        .unwrap_or_else(|| root.join(sanitize_task_id(&task.id.0)))
}

fn owner_record_matches_workspace(
    record: &WorkspaceOwnerRecord,
    task_id: &TaskId,
    workspace_key: &str,
    run_generation: u32,
) -> bool {
    let identity_matches = record
        .workspace_key
        .as_deref()
        .map(|key| key == workspace_key)
        .unwrap_or(record.task_id == task_id.0);
    identity_matches && record.run_generation == run_generation
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
#[path = "workspace_tests.rs"]
mod tests;
