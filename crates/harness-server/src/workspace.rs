use crate::task_runner::{TaskId, TaskSummary};
use dashmap::DashMap;
use harness_core::config::misc::WorkspaceConfig;
use harness_core::types::SessionId;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;
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
mod workspace_helpers;

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
    pub(crate) branch: String,
    pub(crate) created_at: SystemTime,
    pub(crate) owner_session: String,
    pub(crate) run_generation: u32,
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
    git_ops: tokio::sync::Mutex<()>,
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
            git_ops: tokio::sync::Mutex::new(()),
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
        self.cleanup_workspace_path_locked(source_repo, &target)
            .await
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
                match self
                    .cleanup_workspace_path_locked(&cleanup_repo, &path)
                    .await
                {
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

            match self
                .cleanup_workspace_path_locked(&cleanup_repo, path)
                .await
            {
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
                match self.cleanup_workspace_path_locked(source_repo, &path).await {
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

        let mut orphan_paths = Vec::new();
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
            orphan_paths.push(path);
        }

        for path in orphan_paths {
            if self.active.iter().any(|e| e.workspace_path == path) {
                continue;
            }
            let _git_ops = self.git_ops.lock().await;
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
        let _git_ops = self.git_ops.lock().await;
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
#[cfg(test)]
#[path = "workspace_entries_tests.rs"]
mod entries_tests;

#[cfg(test)]
#[path = "workspace_tests.rs"]
mod tests;
