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
        if let Some(active) = self.active.get(task_id) {
            if active.run_generation == run_generation && active.owner_session == self.owner_session
            {
                return Ok(WorkspaceLease {
                    workspace_path: active.workspace_path.clone(),
                    owner_session: active.owner_session.clone(),
                    run_generation,
                    decision: WorkspaceAcquireDecision::ReusedTracked,
                    project_key: active.project_key.clone(),
                    slot_index: active.slot_index,
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

        let pool_permit = match self.pool.acquire(source_repo, repo).await {
            Ok(permit) => permit,
            Err(err) => {
                return Err(WorkspaceLifecycleError::CreateFailed {
                    workspace_path: self.config.root.clone(),
                    workspace_owner: None,
                    message: format!(
                        "workspace pool acquisition failed for task {}: {err}",
                        task_id.0
                    ),
                });
            }
        };
        let project_key = pool_permit.project_key.clone();
        let slot_lock = self.pool.selection_lock(&project_key);
        let slot_guard = slot_lock.lock().await;
        let Some(slot_index) = select_available_slot(
            pool_permit.capacity,
            &self.occupied_slots_for_project(&project_key),
        ) else {
            return Err(WorkspaceLifecycleError::CreateFailed {
                workspace_path: self.config.root.join(&project_key),
                workspace_owner: None,
                message: format!(
                    "workspace pool exhausted for project {project_key}; no free slot after acquiring permit"
                ),
            });
        };
        let workspace_key = workspace_slot_key(&project_key, slot_index);
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
        let owner_record_task_id = external_id.unwrap_or(task_id.0.as_str()).to_string();
        let lease_record = WorkspaceLeaseRecord {
            project_key: project_key.clone(),
            slot_index,
            task_id: task_id.clone(),
            workspace_path: workspace_path.clone(),
            source_repo: source_repo.to_path_buf(),
            repo: repo.map(str::to_owned),
            runtime_workflow_id: options.runtime_workflow_id.clone(),
            owner_session: owner_session.clone(),
            run_generation,
            process_id: std::process::id(),
        };

        {
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
                }
                Entry::Vacant(vac) => {
                    vac.insert(task_id.clone());
                }
            }
        }

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
                            project_key: active.project_key.clone(),
                            slot_index: active.slot_index,
                        });
                    }
                    self.release_active_path(task_id, &workspace_path);
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
                        runtime_workflow_id: options.runtime_workflow_id.clone(),
                        project_key: project_key.clone(),
                        slot_index,
                        branch: branch.clone(),
                        created_at: SystemTime::now(),
                        owner_session: owner_session.clone(),
                        run_generation,
                        _pool_permit: Some(pool_permit.permit),
                    });
                }
            }
        }
        drop(slot_guard);

        if let Some(store) = self.lease_store.as_ref() {
            if let Err(err) = store.upsert_lease(&lease_record).await {
                self.remove_active_workspace(task_id);
                return Err(WorkspaceLifecycleError::CreateFailed {
                    workspace_path: workspace_path.clone(),
                    workspace_owner: Some(owner_session.clone()),
                    message: format!("failed to persist workspace lease: {err}"),
                });
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
                self.release_workspace(task_id).await;
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
                self.release_workspace(task_id).await;
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

        let remote_ref = format!("{remote}/{base_branch}");
        let target_ref = if fetch_output.status.success() {
            remote_ref.as_str()
        } else {
            base_branch
        };

        if workspace_path_exists && is_registered_worktree(source_repo, &workspace_path).await {
            let owner_record = read_owner_record(&workspace_path);
            if self.lease_store.is_none()
                && owner_record
                    .as_ref()
                    .is_some_and(|record| record.owner_session != owner_session)
            {
                self.release_workspace(task_id).await;
                return Err(WorkspaceLifecycleError::LiveForeignOwner {
                    workspace_path: workspace_path.clone(),
                    workspace_owner: owner_record.map(|record| record.owner_session),
                    message: format!(
                        "WorktreeCollision: workspace path {:?} is managed by another harness session; manual resolution required",
                        workspace_path
                    ),
                });
            }
            let can_reuse_without_reset = false;
            if can_reuse_without_reset {
                decision = WorkspaceAcquireDecision::ReusedRecovered;
                tracing::info!(
                    task_id = %task_id.0,
                    path = ?workspace_path,
                    run_generation,
                    "workspace recovery: re-using recorded worktree from current session"
                );
            } else {
                if let Err(err) =
                    reset_registered_worktree(&workspace_path, &branch, target_ref).await
                {
                    self.release_workspace(task_id).await;
                    return Err(WorkspaceLifecycleError::ReconcileFailed {
                        workspace_path: workspace_path.clone(),
                        workspace_owner: owner_record.map(|record| record.owner_session),
                        message: format!(
                            "workspace slot reset failed for task {} at {:?}: {err}",
                            task_id.0, workspace_path
                        ),
                    });
                }
                decision = WorkspaceAcquireDecision::RecreatedStale;
            }

            let owner_record = WorkspaceOwnerRecord {
                task_id: owner_record_task_id.clone(),
                run_generation,
                owner_session: owner_session.clone(),
                workspace_key: Some(workspace_key),
            };
            if let Err(err) = write_owner_record(&workspace_path, &owner_record) {
                self.release_workspace(task_id).await;
                return Err(WorkspaceLifecycleError::CreateFailed {
                    workspace_path: workspace_path.clone(),
                    workspace_owner: Some(owner_session.clone()),
                    message: format!("failed to persist workspace owner record: {err}"),
                });
            }
            drop(git_ops_guard);
            return Ok(WorkspaceLease {
                workspace_path,
                owner_session,
                run_generation,
                decision,
                project_key,
                slot_index,
            });
        }

        if workspace_path_exists || missing_path_registered_worktree {
            let owner_record = read_owner_record(&workspace_path);
            if let Err(err) = cleanup_workspace_path_with_registration(
                source_repo,
                &workspace_path,
                missing_path_registered_worktree.then_some(true),
            )
            .await
            {
                self.release_workspace(task_id).await;
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

        // Create git worktree based on remote/base_branch (latest upstream).
        // Falls back to local base_branch only when strict remote-head
        // admission is disabled.
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
                self.release_workspace(task_id).await;
                return Err(WorkspaceLifecycleError::CreateFailed {
                    workspace_path: workspace_path.clone(),
                    workspace_owner: Some(owner_session.clone()),
                    message: format!("git worktree add failed for task {}: {e}", task_id.0),
                });
            }
        };

        if !output.status.success() {
            if options.require_remote_head {
                self.release_workspace(task_id).await;
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
                    self.release_workspace(task_id).await;
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
                self.release_workspace(task_id).await;
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
            task_id: owner_record_task_id,
            run_generation,
            owner_session: owner_session.clone(),
            workspace_key: Some(workspace_key),
        };
        if let Err(err) = write_owner_record(&workspace_path, &owner_record) {
            self.release_workspace(task_id).await;
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
                self.release_workspace(task_id).await;
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
            project_key,
            slot_index,
        })
    }
}

async fn reset_registered_worktree(
    workspace_path: &Path,
    branch: &str,
    target_ref: &str,
) -> anyhow::Result<()> {
    let checkout = git_command()
        .args([
            "-C",
            &workspace_path.to_string_lossy(),
            "checkout",
            "-B",
            branch,
            target_ref,
        ])
        .output()
        .await?;
    if !checkout.status.success() {
        anyhow::bail!(
            "git checkout -B failed: {}",
            String::from_utf8_lossy(&checkout.stderr).trim()
        );
    }

    let reset = git_command()
        .args([
            "-C",
            &workspace_path.to_string_lossy(),
            "reset",
            "--hard",
            target_ref,
        ])
        .output()
        .await?;
    if !reset.status.success() {
        anyhow::bail!(
            "git reset --hard failed: {}",
            String::from_utf8_lossy(&reset.stderr).trim()
        );
    }

    let clean = git_command()
        .args(["-C", &workspace_path.to_string_lossy(), "clean", "-fdx"])
        .output()
        .await?;
    if !clean.status.success() {
        anyhow::bail!(
            "git clean -fdx failed: {}",
            String::from_utf8_lossy(&clean.stderr).trim()
        );
    }

    Ok(())
}
