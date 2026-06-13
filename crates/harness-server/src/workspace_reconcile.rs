use super::workspace_helpers::*;
use super::*;

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

impl WorkspaceManager {
    pub(crate) async fn reconcile_startup(
        &self,
        source_repo: &Path,
        tasks: &[TaskSummary],
    ) -> anyhow::Result<StartupReconciliation> {
        let mut summary = StartupReconciliation::default();
        if let Some(store) = self.lease_store.as_ref() {
            match store
                .release_foreign_orphaned_leases(&self.owner_session)
                .await
            {
                Ok(released) => {
                    summary.released_leases = released.min(u64::from(u32::MAX)) as u32;
                }
                Err(error) => {
                    tracing::warn!("workspace startup lease reconciliation failed: {error}");
                }
            }
        }
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
            if let Some(store) = self.lease_store.as_ref() {
                match store.leased_workspace_path(&path).await {
                    Ok(Some(record)) => {
                        tracing::debug!(
                            workspace_path = ?path,
                            owner_session = %record.owner_session,
                            task_id = %record.task_id.0,
                            "reconcile_startup: preserving workspace with persisted live lease"
                        );
                        summary.preserved += 1;
                        continue;
                    }
                    Ok(None) => {}
                    Err(error) => {
                        tracing::warn!(
                            workspace_path = ?path,
                            "reconcile_startup: failed to inspect persisted workspace lease: {error}"
                        );
                        summary.preserved += 1;
                        continue;
                    }
                }
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
    /// UUID-keyed dirs (no parseable external_id in the owner record) are skipped: they
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

            // Active workspaces are owned by a running task; do not touch them.
            if self.active.iter().any(|e| e.workspace_path == path) {
                summary.skipped_open += 1;
                continue;
            }
            if let Some(store) = self.lease_store.as_ref() {
                match store.leased_workspace_path(&path).await {
                    Ok(Some(record)) => {
                        tracing::debug!(
                            workspace_path = ?path,
                            owner_session = %record.owner_session,
                            task_id = %record.task_id.0,
                            "reconcile_disk_workspaces: skipping workspace with persisted live lease"
                        );
                        summary.skipped_open += 1;
                        continue;
                    }
                    Ok(None) => {}
                    Err(error) => {
                        tracing::warn!(
                            workspace_path = ?path,
                            "reconcile_disk_workspaces: failed to inspect persisted workspace lease: {error}"
                        );
                        summary.skipped_open += 1;
                        continue;
                    }
                }
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
            //   `{task}-seq`   - sequential run workspace
            //   `{task}-p{N}`  - parallel chunk workspace (N = decimal digits only)
            // A broad `starts_with("{td}-")` would also match unrelated workspaces
            // like `task-42-hotfix`, incorrectly deleting them when `task-42` is
            // terminal. Restricting to the two known suffixes prevents false positives.
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
