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
mod tests {
    use super::*;
    use crate::task_runner::TaskId;
    use std::sync::{Mutex, MutexGuard, OnceLock};

    fn git_command_std() -> std::process::Command {
        let mut cmd = std::process::Command::new(git_binary());
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

    async fn github_state_server(path: &'static str, body: &'static str) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind GitHub mock");
        let addr = listener.local_addr().expect("GitHub mock address");
        tokio::spawn(async move {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let mut buf = [0_u8; 2048];
            let Ok(n) = socket.read(&mut buf).await else {
                return;
            };
            let request = String::from_utf8_lossy(&buf[..n]);
            let (status, response_body) = if request.starts_with(&format!("GET {path} ")) {
                ("200 OK", body)
            } else {
                ("404 Not Found", "{}")
            };
            let response = format!(
                "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{response_body}",
                response_body.len()
            );
            let _ = socket.write_all(response.as_bytes()).await;
        });
        format!("http://{addr}")
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
        run(&[
            "-C",
            &dir.to_string_lossy(),
            "remote",
            "add",
            "origin",
            &dir.to_string_lossy(),
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

    fn test_task_id() -> TaskId {
        harness_core::types::TaskId("550e8400-e29b-41d4-a716-446655440000".to_string())
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
    async fn deterministic_issue_workspace_reuses_existing_directory_for_new_task() {
        let source = tempfile::tempdir().expect("tempdir");
        init_git_repo(source.path());
        let branch = current_branch(source.path());

        let workspaces = tempfile::tempdir().expect("tempdir");
        let config = WorkspaceConfig {
            root: workspaces.path().to_path_buf(),
            auto_cleanup: false,
            ..Default::default()
        };
        let mgr = WorkspaceManager::new(config).expect("new");
        let first_task = harness_core::types::TaskId("task-first".to_string());
        let second_task = harness_core::types::TaskId("task-second".to_string());

        let first = mgr
            .create_workspace(
                &first_task,
                source.path(),
                "origin",
                &branch,
                1,
                Some("issue:42"),
                Some("owner/repo"),
            )
            .await
            .expect("create first workspace");
        let marker = first.workspace_path.join("handoff.txt");
        std::fs::write(&marker, "keep this file").expect("write marker");
        mgr.release_workspace(&first_task);

        let second = mgr
            .create_workspace(
                &second_task,
                source.path(),
                "origin",
                &branch,
                1,
                Some("issue:42"),
                Some("owner/repo"),
            )
            .await
            .expect("reuse deterministic workspace");

        assert_eq!(first.workspace_path, second.workspace_path);
        assert_eq!(second.decision, WorkspaceAcquireDecision::ReusedRecovered);
        assert!(
            second.workspace_path.join("handoff.txt").exists(),
            "reused workspace must preserve prior task output"
        );
        let owner = read_owner_record(&second.workspace_path).expect("owner record");
        assert_eq!(owner.task_id, second_task.0);
        assert!(owner.workspace_key.is_some());

        mgr.remove_workspace(&second_task).await.expect("remove");
    }

    #[tokio::test]
    async fn create_workspace_blocks_inflight_duplicate_deterministic_path() {
        let source = tempfile::tempdir().expect("tempdir");
        init_git_repo(source.path());
        let branch = current_branch(source.path());

        let workspaces = tempfile::tempdir().expect("tempdir");
        let config = WorkspaceConfig {
            root: workspaces.path().to_path_buf(),
            ..Default::default()
        };
        let mgr = WorkspaceManager::new(config).expect("new");
        let first_task = harness_core::types::TaskId("task-first".to_string());
        let second_task = harness_core::types::TaskId("task-second".to_string());
        let workspace_key = derive_workspace_key(
            &first_task,
            Some("issue:42"),
            Some("owner/repo"),
            Some(source.path()),
        );
        let reserved_path = mgr.config.root.join(workspace_key);

        mgr.active_paths
            .insert(reserved_path.clone(), first_task.clone());

        let err = mgr
            .create_workspace(
                &second_task,
                source.path(),
                "origin",
                &branch,
                1,
                Some("issue:42"),
                Some("owner/repo"),
            )
            .await
            .expect_err("second task should be blocked by the path reservation");

        assert!(
            err.to_string().contains("already reserved by active task"),
            "error should identify the in-flight deterministic path owner: {err}"
        );
        assert!(mgr.get_workspace(&second_task).is_none());
        assert_eq!(
            mgr.active_paths
                .get(&reserved_path)
                .map(|owner| owner.value().clone()),
            Some(first_task)
        );
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
        let stale_path = workspaces.path().join("stale-task-check-001");
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

        mgr.create_workspace(&task_id, &source, "origin", &branch, 1, None, None)
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

    /// reconcile_disk_workspaces: UUID-keyed dirs are skipped (not touched).
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
        mgr.release_workspace(&task_id);

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
}
