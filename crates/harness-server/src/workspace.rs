use crate::task_runner::{TaskId, TaskSummary};
use crate::workspace_lease_store::{
    RepositoryLeaseMode, RepositoryLeaseState, RepositoryWriteLease, WorkspaceLeaseRecord,
    WorkspaceLeaseStore,
};
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

#[path = "workspace_active_reuse.rs"]
mod workspace_active_reuse;
pub(crate) use workspace_active_reuse::WorkspaceExecutionGuard;
#[path = "workspace_create.rs"]
mod workspace_create;
#[path = "workspace_helpers.rs"]
pub(crate) mod workspace_helpers;
#[path = "workspace_process.rs"]
mod workspace_process;
#[path = "workspace_reconcile.rs"]
mod workspace_reconcile;
#[path = "workspace_removal.rs"]
mod workspace_removal;
#[path = "workspace_repository.rs"]
mod workspace_repository;
#[path = "workspace_worktree_add.rs"]
mod workspace_worktree_add;

pub(crate) use workspace_helpers::run_hook;
use workspace_helpers::*;
pub(crate) use workspace_removal::WorkspaceRetryCleanupOutcome;
pub(crate) use workspace_repository::{
    run_until_repository_lease_loss, RepositoryWriteLeaseAttempt,
};

fn git_binary() -> String {
    harness_core::config::process_env::var("HARNESS_GIT_BIN").unwrap_or_else(|_| "git".to_string())
}

pub(crate) fn git_command() -> workspace_process::WorkspaceCommand {
    let mut cmd = workspace_process::WorkspaceCommand::new(git_binary(), "workspace-git");
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
    pub(crate) workspace_key: String,
    pub(crate) project_key: String,
    pub(crate) slot_index: u32,
    pub(crate) branch: String,
    pub(crate) created_at: SystemTime,
    pub(crate) owner_session: String,
    pub(crate) run_generation: u32,
    pub(crate) acquisition_id: String,
    pub(crate) state: ActiveWorkspaceState,
    pub(crate) _pool_permit: Option<OwnedSemaphorePermit>,
    pub(crate) _repository_write_lease: Option<RepositoryWriteLease>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ActiveWorkspaceState {
    Creating,
    Ready,
    Preparing,
    Running(String),
    Finalizing(String),
    CleanupRequired,
}

#[derive(Debug, Clone)]
struct ActiveWorkspaceSnapshot {
    workspace_path: PathBuf,
    source_repo: PathBuf,
    workspace_key: String,
    project_key: String,
    slot_index: u32,
    owner_session: String,
    run_generation: u32,
    acquisition_id: String,
}

impl From<&ActiveWorkspace> for ActiveWorkspaceSnapshot {
    fn from(active: &ActiveWorkspace) -> Self {
        Self {
            workspace_path: active.workspace_path.clone(),
            source_repo: active.source_repo.clone(),
            workspace_key: active.workspace_key.clone(),
            project_key: active.project_key.clone(),
            slot_index: active.slot_index,
            owner_session: active.owner_session.clone(),
            run_generation: active.run_generation,
            acquisition_id: active.acquisition_id.clone(),
        }
    }
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

pub(crate) struct WorkspaceCreateOptions {
    pub(crate) require_remote_head: bool,
    pub(crate) reuse_existing_workspace: bool,
    pub(crate) after_create_hook: Option<String>,
    pub(crate) hook_timeout_secs: Option<u64>,
    pub(crate) branch_prefix: String,
    pub(crate) runtime_workflow_id: Option<String>,
    pub(crate) persist_runtime_cleanup_target: bool,
    pub(crate) workspace_capacity_override: Option<usize>,
    pub(crate) repository_write_lease: RepositoryWriteLeaseInput,
}

pub(crate) enum RepositoryWriteLeaseInput {
    ResolveFromStartupConfig,
    NotRequired,
    Held(RepositoryWriteLease),
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
            persist_runtime_cleanup_target: true,
            workspace_capacity_override: None,
            repository_write_lease: RepositoryWriteLeaseInput::ResolveFromStartupConfig,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct WorkspaceOwnerRecord {
    task_id: String,
    run_generation: u32,
    owner_session: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    acquisition_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    workspace_key: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum WorkspaceAcquireDecision {
    CreatedFresh,
    #[cfg(test)]
    ReusedTracked,
    ReusedRecovered,
    RecreatedStale,
}

#[derive(Debug, Clone)]
pub(crate) struct WorkspaceLease {
    pub(crate) workspace_path: PathBuf,
    pub(crate) acquisition_id: String,
    pub(crate) repository_lease_lost: Option<tokio::sync::watch::Receiver<RepositoryLeaseState>>,
    #[cfg(test)]
    pub(crate) owner_session: String,
    #[cfg(test)]
    pub(crate) run_generation: u32,
    #[cfg(test)]
    pub(crate) decision: WorkspaceAcquireDecision,
    #[cfg(test)]
    pub(crate) project_key: String,
    #[cfg(test)]
    pub(crate) slot_index: u32,
}

#[derive(Debug, Clone)]
pub(crate) enum WorkspaceLifecycleError {
    LiveForeignOwner { message: String },
    ReconcileFailed { message: String },
    CreateFailed { message: String },
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SkippedLiveWorkspace {
    pub(crate) path: PathBuf,
    pub(crate) task_id: TaskId,
    pub(crate) owner_session: String,
}

/// Summary produced by the periodic disk reconciliation scan.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct DiskReconciliationSummary {
    pub(crate) scanned: u32,
    pub(crate) removed: u32,
    pub(crate) skipped_uuid: u32,
    pub(crate) skipped_open: u32,
    pub(crate) skipped_live: Vec<SkippedLiveWorkspace>,
    pub(crate) released_leases: u32,
    pub(crate) errors: u32,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct OrphanWorkspaceCleanupSummary {
    pub removed: u32,
    pub skipped_live: u32,
    pub deferred: u32,
    pub errors: u32,
    pub prune_deferred: bool,
}

pub struct WorkspaceManager {
    pub(crate) config: WorkspaceConfig,
    pub(crate) active: Arc<DashMap<TaskId, ActiveWorkspace>>,
    pub(crate) active_paths: Arc<DashMap<PathBuf, TaskId>>,
    released_paths: Arc<DashMap<TaskId, PathBuf>>,
    released_workspace_paths: Arc<DashMap<String, PathBuf>>,
    pub(crate) owner_session: String,
    git_ops: Arc<tokio::sync::Mutex<()>>,
    pool: WorkspacePool,
    lease_store: Option<Arc<WorkspaceLeaseStore>>,
    capacity_source: Option<WorkspaceCapacitySource>,
}

struct WorkspaceCapacitySource {
    server: Arc<crate::server::HarnessServer>,
    project_registry: Option<Arc<crate::project_registry::ProjectRegistry>>,
}

impl WorkspaceManager {
    pub fn new(config: WorkspaceConfig) -> anyhow::Result<Self> {
        #[cfg(not(test))]
        let pool_config = WorkspacePoolConfig::default();
        #[cfg(test)]
        let pool_config =
            WorkspacePoolConfig::new_for_local_pool_tests(4, std::collections::HashMap::new());
        Self::new_with_pool(config, pool_config, None)
    }

    pub(crate) fn new_with_pool(
        config: WorkspaceConfig,
        pool_config: WorkspacePoolConfig,
        lease_store: Option<Arc<WorkspaceLeaseStore>>,
    ) -> anyhow::Result<Self> {
        Self::new_with_pool_inner(config, pool_config, lease_store, None)
    }

    pub(crate) fn new_with_pool_and_capacity_source(
        config: WorkspaceConfig,
        pool_config: WorkspacePoolConfig,
        lease_store: Option<Arc<WorkspaceLeaseStore>>,
        server: Arc<crate::server::HarnessServer>,
        project_registry: Option<Arc<crate::project_registry::ProjectRegistry>>,
    ) -> anyhow::Result<Self> {
        Self::new_with_pool_inner(
            config,
            pool_config,
            lease_store,
            Some(WorkspaceCapacitySource {
                server,
                project_registry,
            }),
        )
    }

    fn new_with_pool_inner(
        mut config: WorkspaceConfig,
        pool_config: WorkspacePoolConfig,
        lease_store: Option<Arc<WorkspaceLeaseStore>>,
        capacity_source: Option<WorkspaceCapacitySource>,
    ) -> anyhow::Result<Self> {
        if !config.root.is_absolute() {
            config.root = std::env::current_dir()?.join(&config.root);
        }
        std::fs::create_dir_all(&config.root)?;
        Ok(Self {
            config,
            active: Arc::new(DashMap::new()),
            active_paths: Arc::new(DashMap::new()),
            released_paths: Arc::new(DashMap::new()),
            released_workspace_paths: Arc::new(DashMap::new()),
            owner_session: SessionId::new().to_string(),
            git_ops: Arc::new(tokio::sync::Mutex::new(())),
            pool: WorkspacePool::new(pool_config),
            lease_store,
            capacity_source,
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

    fn occupied_slots_for_project(&self, project_key: &str) -> HashSet<u32> {
        self.active
            .iter()
            .filter(|entry| entry.project_key == project_key)
            .map(|entry| entry.slot_index)
            .collect()
    }

    async fn release_persisted_lease(
        &self,
        task_id: &TaskId,
        entry: &ActiveWorkspace,
    ) -> anyhow::Result<()> {
        let Some(store) = self.lease_store.as_ref() else {
            return Ok(());
        };
        let released = store
            .release_owned_slot(
                &entry.project_key,
                entry.slot_index,
                task_id,
                &entry.owner_session,
                entry.run_generation,
                &entry.acquisition_id,
            )
            .await?;
        if !released {
            anyhow::bail!(
                "persisted workspace acquisition was not leased for task {}",
                task_id.0
            );
        }
        Ok(())
    }

    async fn cleanup_workspace_path_locked(
        &self,
        source_repo: &Path,
        workspace_path: &Path,
    ) -> anyhow::Result<()> {
        let _git_ops = self.git_ops.lock().await;
        cleanup_workspace_path(source_repo, workspace_path).await
    }

    /// Release the in-memory lease without deleting the workspace on disk.
    ///
    /// Used when `auto_cleanup=false` so a later task with the same deterministic
    /// issue/PR workspace key can reuse the directory while concurrent tasks are
    /// still protected by the active-path collision check.
    pub async fn release_workspace(&self, task_id: &TaskId) {
        let acquisition_id = self
            .active
            .get(task_id)
            .map(|active| active.acquisition_id.clone());
        if let Some(acquisition_id) = acquisition_id {
            if let Err(error) = self
                .release_workspace_acquisition(task_id, &acquisition_id)
                .await
            {
                tracing::warn!(task_id = %task_id.0, "failed to release workspace: {error}");
            }
        }
    }

    pub(crate) async fn release_workspace_acquisition(
        &self,
        task_id: &TaskId,
        acquisition_id: &str,
    ) -> anyhow::Result<()> {
        let entry = self
            .active
            .remove_if(task_id, |_, active| active.acquisition_id == acquisition_id)
            .map(|(_, active)| active);
        if let Some(entry) = entry {
            if let Err(error) = self.release_persisted_lease(task_id, &entry).await {
                self.active.entry(task_id.clone()).or_insert(entry);
                return Err(error);
            }
            self.release_active_path(task_id, &entry.workspace_path);
            self.released_paths
                .insert(task_id.clone(), entry.workspace_path.clone());
            self.released_workspace_paths
                .insert(entry.workspace_key.clone(), entry.workspace_path.clone());
        }
        Ok(())
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
        let workspace_key = derive_workspace_key(task_id, external_id, repo, Some(source_repo));
        if let Some(path) = self
            .released_workspace_paths
            .get(&workspace_key)
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
#[path = "workspace_test_support.rs"]
pub(crate) mod test_support;

#[cfg(test)]
#[path = "workspace_disk_reconcile_tests.rs"]
mod disk_reconcile_tests;

#[cfg(test)]
#[path = "workspace_lease_store_tests.rs"]
mod lease_store_tests;

#[cfg(test)]
#[path = "workspace_pool_tests.rs"]
mod pool_tests;

#[cfg(test)]
#[path = "workspace_reclaim_tests.rs"]
mod reclaim_tests;

#[cfg(test)]
#[path = "workspace_startup_reconcile_tests.rs"]
mod startup_reconcile_tests;

#[cfg(test)]
#[path = "workspace_tests.rs"]
mod tests;
