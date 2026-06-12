use crate::task_runner::{TaskId, TaskSummary};
use crate::workspace_lease_store::{WorkspaceLeaseRecord, WorkspaceLeaseStore};
use crate::workspace_pool::{
    select_available_slot, workspace_slot_key, WorkspacePool, WorkspacePoolConfig,
};
use dashmap::DashMap;
use harness_core::config::misc::WorkspaceConfig;
use harness_core::types::SessionId;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;
use tokio::sync::OwnedSemaphorePermit;
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

#[path = "workspace_create.rs"]
mod workspace_create;
#[path = "workspace_helpers.rs"]
pub(crate) mod workspace_helpers;
#[path = "workspace_reconcile.rs"]
mod workspace_reconcile;

pub(crate) use workspace_helpers::run_hook;
use workspace_helpers::*;

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
    pub(crate) repo: Option<String>,
    pub(crate) runtime_workflow_id: Option<String>,
    pub(crate) project_key: String,
    pub(crate) slot_index: u32,
    pub(crate) branch: String,
    pub(crate) created_at: SystemTime,
    pub(crate) owner_session: String,
    pub(crate) run_generation: u32,
    pub(crate) _pool_permit: Option<OwnedSemaphorePermit>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceEntry {
    pub task_id: TaskId,
    pub workspace_path: PathBuf,
    pub source_repo: PathBuf,
    pub repo: Option<String>,
    pub runtime_workflow_id: Option<String>,
    pub branch: String,
    pub created_at: SystemTime,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WorkspaceCreateOptions {
    pub(crate) require_remote_head: bool,
    pub(crate) reuse_existing_workspace: bool,
    pub(crate) after_create_hook: Option<String>,
    pub(crate) hook_timeout_secs: Option<u64>,
    pub(crate) branch_prefix: String,
    pub(crate) runtime_workflow_id: Option<String>,
}

impl Default for WorkspaceCreateOptions {
    fn default() -> Self {
        Self {
            require_remote_head: true,
            reuse_existing_workspace: true,
            after_create_hook: None,
            hook_timeout_secs: None,
            branch_prefix: "harness/".to_string(),
            runtime_workflow_id: None,
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
    pub(crate) project_key: String,
    pub(crate) slot_index: u32,
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
    pub(crate) released_leases: u32,
}

/// Summary produced by the periodic disk reconciliation scan.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DiskReconciliationSummary {
    pub(crate) scanned: u32,
    pub(crate) removed: u32,
    pub(crate) skipped_uuid: u32,
    pub(crate) skipped_open: u32,
}

pub struct WorkspaceManager {
    pub(crate) config: WorkspaceConfig,
    pub(crate) active: DashMap<TaskId, ActiveWorkspace>,
    pub(crate) active_paths: DashMap<PathBuf, TaskId>,
    released_paths: DashMap<TaskId, PathBuf>,
    pub(crate) owner_session: String,
    git_ops: tokio::sync::Mutex<()>,
    pool: WorkspacePool,
    lease_store: Option<Arc<WorkspaceLeaseStore>>,
}

impl WorkspaceManager {
    pub fn new(config: WorkspaceConfig) -> anyhow::Result<Self> {
        Self::new_with_pool(config, WorkspacePoolConfig::default(), None)
    }

    pub(crate) fn new_with_pool(
        mut config: WorkspaceConfig,
        pool_config: WorkspacePoolConfig,
        lease_store: Option<Arc<WorkspaceLeaseStore>>,
    ) -> anyhow::Result<Self> {
        if !config.root.is_absolute() {
            config.root = std::env::current_dir()?.join(&config.root);
        }
        std::fs::create_dir_all(&config.root)?;
        Ok(Self {
            config,
            active: DashMap::new(),
            active_paths: DashMap::new(),
            released_paths: DashMap::new(),
            owner_session: SessionId::new().to_string(),
            git_ops: tokio::sync::Mutex::new(()),
            pool: WorkspacePool::new(pool_config),
            lease_store,
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

    fn occupied_slots_for_project(&self, project_key: &str) -> HashSet<u32> {
        self.active
            .iter()
            .filter(|entry| entry.project_key == project_key)
            .map(|entry| entry.slot_index)
            .collect()
    }

    async fn release_persisted_lease(&self, task_id: &TaskId, entry: &ActiveWorkspace) {
        let Some(store) = self.lease_store.as_ref() else {
            return;
        };
        if let Err(error) = store
            .release_slot(&entry.project_key, entry.slot_index, task_id)
            .await
        {
            tracing::warn!(
                task_id = %task_id.0,
                project_key = %entry.project_key,
                slot_index = entry.slot_index,
                "failed to release persisted workspace lease: {error}"
            );
        }
    }

    async fn cleanup_workspace_path_locked(
        &self,
        source_repo: &Path,
        workspace_path: &Path,
    ) -> anyhow::Result<()> {
        let _git_ops = self.git_ops.lock().await;
        cleanup_workspace_path(source_repo, workspace_path).await
    }

    /// Remove the workspace for the given task. Runs `before_remove_hook` first (non-fatal).
    /// Idempotent: returns Ok if the task has no active workspace.
    pub async fn remove_workspace(&self, task_id: &TaskId) -> anyhow::Result<()> {
        let entry = match self.remove_active_workspace(task_id) {
            Some(entry) => entry,
            None => return Ok(()),
        };
        self.released_paths.remove(task_id);
        self.release_persisted_lease(task_id, &entry).await;

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

        if let Err(e) = self
            .cleanup_workspace_path_locked(&entry.source_repo, &entry.workspace_path)
            .await
        {
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
    pub async fn release_workspace(&self, task_id: &TaskId) {
        if let Some(entry) = self.remove_active_workspace(task_id) {
            self.released_paths
                .insert(task_id.clone(), entry.workspace_path.clone());
            self.release_persisted_lease(task_id, &entry).await;
        } else if let Some(store) = self.lease_store.as_ref() {
            if let Err(error) = store.release_task(task_id).await {
                tracing::warn!(
                    task_id = %task_id.0,
                    "failed to release persisted workspace lease for inactive task: {error}"
                );
            }
        }
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
        if let Some(entry) = self.remove_active_workspace(task_id) {
            self.release_persisted_lease(task_id, &entry).await;
        }
        self.released_paths.remove(task_id);
        if let Some(owner_task) = self.active_paths.get(&target) {
            tracing::warn!(
                task_id = %task_id.0,
                owner_task_id = %owner_task.value().0,
                workspace_path = ?target,
                "cleanup_workspace_for_retry: skipped deleting workspace claimed during retry cleanup"
            );
            return Ok(());
        }
        self.cleanup_workspace_path_locked(source_repo, &target)
            .await
    }

    pub(crate) fn workspace_path_for(
        &self,
        task_id: &TaskId,
        source_repo: &Path,
        external_id: Option<&str>,
        repo: Option<&str>,
    ) -> PathBuf {
        self.config.root.join(derive_workspace_key(
            task_id,
            external_id,
            repo,
            Some(source_repo),
        ))
    }

    pub(crate) async fn workspace_path_for_cleanup(
        &self,
        task_id: &TaskId,
        source_repo: &Path,
        external_id: Option<&str>,
        repo: Option<&str>,
    ) -> PathBuf {
        if let Some(path) = self.get_workspace(task_id) {
            return path;
        }
        if let Some(path) = self
            .released_paths
            .get(task_id)
            .map(|entry| entry.value().clone())
        {
            return path;
        }
        if let Some(store) = self.lease_store.as_ref() {
            match store.latest_workspace_path_for_task(task_id).await {
                Ok(Some(path)) => return path,
                Ok(None) => {}
                Err(error) => {
                    tracing::warn!(
                        task_id = %task_id.0,
                        "failed to resolve workspace cleanup path from lease store: {error}"
                    );
                }
            }
        }
        self.workspace_path_for(task_id, source_repo, external_id, repo)
    }

    /// Return the workspace path for the given task if it is active.
    pub fn get_workspace(&self, task_id: &TaskId) -> Option<PathBuf> {
        self.active.get(task_id).map(|e| e.workspace_path.clone())
    }

    /// Number of worktrees currently checked out and not yet reaped.
    pub fn live_count(&self) -> u64 {
        self.active.len() as u64
    }

    pub fn entries(&self) -> Vec<WorkspaceEntry> {
        let mut entries = self
            .active
            .iter()
            .map(|entry| WorkspaceEntry {
                task_id: entry.key().clone(),
                workspace_path: entry.workspace_path.clone(),
                source_repo: entry.source_repo.clone(),
                repo: entry.repo.clone(),
                runtime_workflow_id: entry.runtime_workflow_id.clone(),
                branch: entry.branch.clone(),
                created_at: entry.created_at,
            })
            .collect::<Vec<_>>();
        entries.sort_by(|left, right| left.task_id.0.cmp(&right.task_id.0));
        entries
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
#[cfg(test)]
#[path = "workspace_entries_tests.rs"]
mod entries_tests;

#[cfg(test)]
#[path = "workspace_tests.rs"]
mod tests;
