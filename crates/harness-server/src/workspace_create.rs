use super::workspace_helpers::*;
use super::*;

impl WorkspaceManager {
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
        let branch = format!("{}{}", options.branch_prefix, task_id.0);
        if !is_valid_branch_name(&branch) {
            return Err(WorkspaceLifecycleError::CreateFailed {
                workspace_path: workspace_path.clone(),
                workspace_owner: None,
                message: format!("invalid workspace branch: {branch:?}"),
            });
        }
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
                        repo: repo.map(str::to_owned),
                        branch: branch.clone(),
                        created_at: SystemTime::now(),
                        owner_session: owner_session.clone(),
                        run_generation,
                    });
                }
            }
        }

        let git_ops_guard = self.git_ops.lock().await;
        let mut decision = WorkspaceAcquireDecision::CreatedFresh;
        let workspace_path_exists = workspace_path.exists();
        let missing_path_registered_worktree = if workspace_path_exists {
            false
        } else {
            is_registered_worktree(source_repo, &workspace_path).await
        };

        if workspace_path_exists || missing_path_registered_worktree {
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

            if let Err(err) = cleanup_workspace_path_with_registration(
                source_repo,
                &workspace_path,
                missing_path_registered_worktree.then_some(true),
            )
            .await
            {
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
        drop(git_ops_guard);

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
                if let Err(cleanup_err) = self
                    .cleanup_workspace_path_locked(source_repo, &workspace_path)
                    .await
                {
                    tracing::warn!(
                        "failed to cleanup partial worktree after hook failure: {cleanup_err}"
                    );
                }
                self.remove_active_workspace(task_id);
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
}
