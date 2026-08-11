use super::workspace_helpers::*;
use super::*;

const PERSISTED_SLOT_RETRY_DELAY: Duration = Duration::from_millis(250);

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
                    #[cfg(test)]
                    owner_session: active.owner_session.clone(),
                    #[cfg(test)]
                    run_generation,
                    #[cfg(test)]
                    decision: WorkspaceAcquireDecision::ReusedTracked,
                    #[cfg(test)]
                    project_key: active.project_key.clone(),
                    #[cfg(test)]
                    slot_index: active.slot_index,
                });
            }
            return Err(WorkspaceLifecycleError::LiveForeignOwner {
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
                    message: format!(
                        "workspace pool acquisition failed for task {}: {err}",
                        task_id.0
                    ),
                });
            }
        };
        let project_key = pool_permit.project_key.clone();
        let workspace_project_key = pool_permit.workspace_project_key.clone();
        let capacity = pool_permit.capacity;
        let mut pool_permit = Some(pool_permit.permit);
        let repository_write_lease = self
            .acquire_repository_write_lease_if_single_writer(source_repo)
            .await
            .map_err(|err| WorkspaceLifecycleError::CreateFailed {
                message: format!(
                    "failed to acquire PostgreSQL repository write lease for {project_key}: {err}"
                ),
            })?;
        let slot_lock = self.pool.selection_lock(&project_key);
        let slot_guard = slot_lock.lock().await;
        if let Some(active) = self.active.get(task_id) {
            if active.run_generation == run_generation && active.owner_session == self.owner_session
            {
                return Ok(WorkspaceLease {
                    workspace_path: active.workspace_path.clone(),
                    #[cfg(test)]
                    owner_session: active.owner_session.clone(),
                    #[cfg(test)]
                    run_generation,
                    #[cfg(test)]
                    decision: WorkspaceAcquireDecision::ReusedTracked,
                    #[cfg(test)]
                    project_key: active.project_key.clone(),
                    #[cfg(test)]
                    slot_index: active.slot_index,
                });
            }
            return Err(WorkspaceLifecycleError::LiveForeignOwner {
                message: format!(
                    "WorktreeCollision: workspace path {:?} already owned by another harness session; manual resolution required",
                    active.workspace_path
                ),
            });
        }
        // Validate inputs to prevent unexpected git behavior.
        if !is_valid_branch_name(base_branch) {
            return Err(WorkspaceLifecycleError::CreateFailed {
                message: format!("invalid base_branch: {base_branch:?}"),
            });
        }
        if !is_valid_branch_name(remote) {
            return Err(WorkspaceLifecycleError::CreateFailed {
                message: format!("invalid remote: {remote:?}"),
            });
        }

        let branch = format!("{}{}", options.branch_prefix, task_id.0);
        if !is_valid_branch_name(&branch) {
            return Err(WorkspaceLifecycleError::CreateFailed {
                message: format!("invalid workspace branch: {branch:?}"),
            });
        }
        let owner_session = self.owner_session.clone();
        let owner_record_task_id = external_id.unwrap_or(task_id.0.as_str()).to_string();
        let owner_record_workspace_key =
            derive_workspace_key(task_id, external_id, repo, Some(source_repo));
        let released_path_for_task = if options.reuse_existing_workspace {
            self.released_paths
                .get(task_id)
                .map(|entry| entry.value().clone())
        } else {
            None
        };
        let released_path_for_workspace_key = if options.reuse_existing_workspace {
            self.released_workspace_paths
                .get(&owner_record_workspace_key)
                .map(|entry| entry.value().clone())
        } else {
            None
        };
        let preferred_released_lease = if options.reuse_existing_workspace {
            if let Some(store) = self.lease_store.as_ref() {
                match store
                    .latest_released_lease_for_workspace_key(
                        &project_key,
                        &owner_record_workspace_key,
                    )
                    .await
                {
                    Ok(record) => record.filter(|record| {
                        record.project_key == project_key && record.slot_index < capacity as u32
                    }),
                    Err(err) => {
                        return Err(WorkspaceLifecycleError::CreateFailed {
                            message: format!(
                                "failed to inspect released workspace lease for workspace key {}: {err}",
                                owner_record_workspace_key
                            ),
                        });
                    }
                }
            } else {
                None
            }
        } else {
            None
        };
        let mut preferred_released_slot = preferred_released_lease
            .as_ref()
            .map(|record| record.slot_index)
            .or_else(|| {
                released_path_for_task
                    .as_deref()
                    .and_then(|path| slot_index_from_workspace_path(&workspace_project_key, path))
                    .filter(|slot| *slot < capacity as u32)
            })
            .or_else(|| {
                released_path_for_workspace_key
                    .as_deref()
                    .and_then(|path| slot_index_from_workspace_path(&workspace_project_key, path))
                    .filter(|slot| *slot < capacity as u32)
            });
        let wait_for_persisted_slot = self.lease_store.is_some();
        let (slot_index, _workspace_key, workspace_path, reacquired_released_task_slot) = loop {
            let mut occupied_slots = self.occupied_slots_for_project(&project_key);
            let persisted_occupied_slots = if let Some(store) = self.lease_store.as_ref() {
                match store.leased_slots_for_project(&project_key).await {
                    Ok(slots) => slots,
                    Err(err) => {
                        return Err(WorkspaceLifecycleError::CreateFailed {
                            message: format!(
                                "failed to inspect persisted workspace leases for project {project_key}: {err}"
                            ),
                        });
                    }
                }
            } else {
                std::collections::HashSet::new()
            };
            occupied_slots.extend(persisted_occupied_slots.iter().copied());
            let preferred_slot =
                preferred_released_slot.filter(|slot| !occupied_slots.contains(slot));
            let Some(slot_index) =
                preferred_slot.or_else(|| select_available_slot(capacity, &occupied_slots))
            else {
                if wait_for_persisted_slot {
                    tracing::debug!(
                        task_id = %task_id.0,
                        project_key = %project_key,
                        capacity,
                        "workspace pool waiting for a persisted slot to be released"
                    );
                    tokio::time::sleep(PERSISTED_SLOT_RETRY_DELAY).await;
                    continue;
                }
                return Err(WorkspaceLifecycleError::CreateFailed {
                    message: format!(
                        "workspace pool exhausted for project {project_key}; no free persisted slot after acquiring permit"
                    ),
                });
            };
            let workspace_key = workspace_slot_key(&workspace_project_key, slot_index);
            let workspace_path = self.config.root.join(&workspace_key);
            let selected_released_task_slot = preferred_slot == Some(slot_index);
            let process_started_at = if self.lease_store.is_some() {
                WorkspaceLeaseStore::current_process_started_at().map_err(|err| {
                    WorkspaceLifecycleError::CreateFailed {
                        message: format!("failed to identify workspace lease process: {err}"),
                    }
                })?
            } else {
                0
            };
            let lease_record = WorkspaceLeaseRecord {
                project_key: project_key.clone(),
                slot_index,
                task_id: task_id.clone(),
                workspace_path: workspace_path.clone(),
                source_repo: source_repo.to_path_buf(),
                repo: repo.map(str::to_owned),
                runtime_workflow_id: options.runtime_workflow_id.clone(),
                workspace_key: owner_record_workspace_key.clone(),
                owner_session: owner_session.clone(),
                run_generation,
                process_id: std::process::id(),
                process_started_at,
            };
            let Some(store) = self.lease_store.as_ref() else {
                break (
                    slot_index,
                    workspace_key,
                    workspace_path,
                    selected_released_task_slot,
                );
            };
            match store.try_acquire_lease(&lease_record).await {
                Ok(true) => {
                    break (
                        slot_index,
                        workspace_key,
                        workspace_path,
                        selected_released_task_slot,
                    )
                }
                Ok(false) => {
                    if preferred_released_slot == Some(slot_index) {
                        preferred_released_slot = None;
                    }
                    tokio::task::yield_now().await;
                }
                Err(err) => {
                    return Err(WorkspaceLifecycleError::CreateFailed {
                        message: format!("failed to persist workspace lease: {err}"),
                    });
                }
            }
        };

        let path_tracking_error = {
            use dashmap::mapref::entry::Entry;
            match self.active_paths.entry(workspace_path.clone()) {
                Entry::Occupied(occ) => {
                    let owner_task = occ.get().clone();
                    if owner_task != task_id.clone() {
                        let existing_path = self
                            .active
                            .get(&owner_task)
                            .map(|entry| entry.workspace_path.clone())
                            .unwrap_or_else(|| workspace_path.clone());
                        Some(WorkspaceLifecycleError::LiveForeignOwner {
                            message: format!(
                                "WorktreeCollision: workspace path {:?} already reserved by active task {}; manual resolution required",
                                existing_path,
                                owner_task.0
                            ),
                        })
                    } else {
                        None
                    }
                }
                Entry::Vacant(vac) => {
                    vac.insert(task_id.clone());
                    None
                }
            }
        };
        if let Some(error) = path_tracking_error {
            if let Some(store) = self.lease_store.as_ref() {
                if let Err(release_error) =
                    store.release_slot(&project_key, slot_index, task_id).await
                {
                    tracing::warn!(
                        task_id = %task_id.0,
                        project_key = %project_key,
                        slot_index,
                        "failed to release persisted workspace lease after path collision: {release_error}"
                    );
                }
            }
            return Err(error);
        }

        let active_tracking_error = {
            use dashmap::mapref::entry::Entry;
            match self.active.entry(task_id.clone()) {
                Entry::Occupied(occ) => {
                    let active = occ.get();
                    let active_path = active.workspace_path.clone();
                    self.release_active_path(task_id, &workspace_path);
                    Some(WorkspaceLifecycleError::LiveForeignOwner {
                        message: format!(
                            "WorktreeCollision: workspace path {:?} already owned by another harness session; manual resolution required",
                            active_path
                        ),
                    })
                }
                Entry::Vacant(vac) => {
                    vac.insert(ActiveWorkspace {
                        workspace_path: workspace_path.clone(),
                        source_repo: source_repo.to_path_buf(),
                        repo: repo.map(str::to_owned),
                        runtime_workflow_id: options.runtime_workflow_id.clone(),
                        workspace_key: owner_record_workspace_key.clone(),
                        project_key: project_key.clone(),
                        slot_index,
                        branch: branch.clone(),
                        created_at: SystemTime::now(),
                        owner_session: owner_session.clone(),
                        run_generation,
                        _pool_permit: pool_permit.take(),
                        _repository_write_lease: repository_write_lease,
                    });
                    None
                }
            }
        };
        if let Some(error) = active_tracking_error {
            if let Some(store) = self.lease_store.as_ref() {
                if let Err(release_error) =
                    store.release_slot(&project_key, slot_index, task_id).await
                {
                    tracing::warn!(
                        task_id = %task_id.0,
                        project_key = %project_key,
                        slot_index,
                        "failed to release persisted workspace lease after active collision: {release_error}"
                    );
                }
            }
            return Err(error);
        }
        drop(slot_guard);

        let git_ops_guard = self.git_ops.lock().await;
        let mut _decision = WorkspaceAcquireDecision::CreatedFresh;
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
                    message: format!("git fetch failed for task {}: {e}", task_id.0),
                });
            }
        };
        let reacquired_released_path = released_path_for_task
            .as_ref()
            .is_some_and(|path| path == &workspace_path)
            || released_path_for_workspace_key
                .as_ref()
                .is_some_and(|path| path == &workspace_path);

        if !fetch_output.status.success() {
            let stderr = String::from_utf8_lossy(&fetch_output.stderr);
            if options.require_remote_head {
                self.release_workspace(task_id).await;
                return Err(WorkspaceLifecycleError::CreateFailed {
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
                    message: format!(
                        "WorktreeCollision: workspace path {:?} is managed by another harness session; manual resolution required",
                        workspace_path
                    ),
                });
            }
            let owner_record_matches_current_workspace =
                owner_record.as_ref().is_some_and(|record| {
                    record.task_id == owner_record_task_id
                        || record.workspace_key.as_deref()
                            == Some(owner_record_workspace_key.as_str())
                });
            let can_reuse_without_reset = options.reuse_existing_workspace
                && (reacquired_released_task_slot || reacquired_released_path)
                && owner_record_matches_current_workspace;
            if can_reuse_without_reset {
                _decision = WorkspaceAcquireDecision::ReusedRecovered;
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
                    if let Err(cleanup_err) = self
                        .cleanup_created_workspace_then_release(
                            task_id,
                            source_repo,
                            &workspace_path,
                            Some(true),
                        )
                        .await
                    {
                        tracing::warn!(
                            "failed to cleanup workspace after slot reset failure: {cleanup_err}"
                        );
                    }
                    return Err(WorkspaceLifecycleError::ReconcileFailed {
                        message: format!(
                            "workspace slot reset failed for task {} at {:?}: {err}",
                            task_id.0, workspace_path
                        ),
                    });
                }
                _decision = WorkspaceAcquireDecision::RecreatedStale;
            }

            let owner_record = WorkspaceOwnerRecord {
                task_id: owner_record_task_id.clone(),
                run_generation,
                owner_session: owner_session.clone(),
                workspace_key: Some(owner_record_workspace_key.clone()),
            };
            if let Err(err) = write_owner_record(&workspace_path, &owner_record) {
                self.release_workspace(task_id).await;
                return Err(WorkspaceLifecycleError::CreateFailed {
                    message: format!("failed to persist workspace owner record: {err}"),
                });
            }
            drop(git_ops_guard);
            return Ok(WorkspaceLease {
                workspace_path,
                #[cfg(test)]
                owner_session,
                #[cfg(test)]
                run_generation,
                #[cfg(test)]
                decision: _decision,
                #[cfg(test)]
                project_key,
                #[cfg(test)]
                slot_index,
            });
        }

        if workspace_path_exists || missing_path_registered_worktree {
            if let Err(err) = cleanup_workspace_path_with_registration(
                source_repo,
                &workspace_path,
                missing_path_registered_worktree.then_some(true),
            )
            .await
            {
                self.release_workspace(task_id).await;
                return Err(WorkspaceLifecycleError::ReconcileFailed {
                    message: format!(
                        "workspace lifecycle reconciliation failed for task {} at {:?}: {err}",
                        task_id.0, workspace_path
                    ),
                });
            }
            _decision = WorkspaceAcquireDecision::RecreatedStale;
        }

        // Create git worktree based on remote/base_branch (latest upstream),
        // with class-targeted recovery for stale admin entries and lock
        // contention (GH-1886). Falls back to local base_branch only when
        // strict remote-head admission is disabled.
        if let Err(error) = workspace_worktree_add::worktree_add_with_recovery(
            source_repo,
            &branch,
            &workspace_path,
            &remote_ref,
        )
        .await
        {
            match error {
                workspace_worktree_add::WorktreeAddError::Spawn(e) => {
                    if let Err(cleanup_err) = self
                        .cleanup_created_workspace_then_release(
                            task_id,
                            source_repo,
                            &workspace_path,
                            None,
                        )
                        .await
                    {
                        tracing::warn!(
                            "failed to cleanup partial worktree after worktree-add spawn failure: {cleanup_err}"
                        );
                    }
                    return Err(WorkspaceLifecycleError::CreateFailed {
                        message: format!("git worktree add failed for task {}: {e}", task_id.0),
                    });
                }
                workspace_worktree_add::WorktreeAddError::Failed(failure) => {
                    if options.require_remote_head {
                        if let Err(cleanup_err) = self
                            .cleanup_created_workspace_then_release(
                                task_id,
                                source_repo,
                                &workspace_path,
                                None,
                            )
                            .await
                        {
                            tracing::warn!(
                                "failed to cleanup partial worktree after worktree-add failure: {cleanup_err}"
                            );
                        }
                        return Err(WorkspaceLifecycleError::CreateFailed {
                            message: format!(
                                "git worktree add from {remote_ref} failed for task {}: {}",
                                task_id.0,
                                failure.evidence()
                            ),
                        });
                    }
                    // Fallback: try local base_branch (useful for repos without remotes, e.g. tests).
                    if let Err(fallback_error) = workspace_worktree_add::worktree_add_with_recovery(
                        source_repo,
                        &branch,
                        &workspace_path,
                        base_branch,
                    )
                    .await
                    {
                        if let Err(cleanup_err) = self
                            .cleanup_created_workspace_then_release(
                                task_id,
                                source_repo,
                                &workspace_path,
                                None,
                            )
                            .await
                        {
                            tracing::warn!(
                                "failed to cleanup partial worktree after fallback worktree-add failure: {cleanup_err}"
                            );
                        }
                        let message = match fallback_error {
                            workspace_worktree_add::WorktreeAddError::Spawn(e) => format!(
                                "git worktree add fallback failed for task {}: {e}",
                                task_id.0
                            ),
                            workspace_worktree_add::WorktreeAddError::Failed(failure) => {
                                format!(
                                    "git worktree add failed for task {}: {}",
                                    task_id.0,
                                    failure.evidence()
                                )
                            }
                        };
                        return Err(WorkspaceLifecycleError::CreateFailed { message });
                    }
                }
            }
        }

        let owner_record = WorkspaceOwnerRecord {
            task_id: owner_record_task_id,
            run_generation,
            owner_session: owner_session.clone(),
            workspace_key: Some(owner_record_workspace_key),
        };
        if let Err(err) = write_owner_record(&workspace_path, &owner_record) {
            if let Err(cleanup_err) = self
                .cleanup_created_workspace_then_release(task_id, source_repo, &workspace_path, None)
                .await
            {
                tracing::warn!(
                    "failed to cleanup partial worktree after owner-record failure: {cleanup_err}"
                );
            }
            return Err(WorkspaceLifecycleError::CreateFailed {
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
                return Err(WorkspaceLifecycleError::CreateFailed { message: err_msg });
            }
        }

        Ok(WorkspaceLease {
            workspace_path,
            #[cfg(test)]
            owner_session,
            #[cfg(test)]
            run_generation,
            #[cfg(test)]
            decision: _decision,
            #[cfg(test)]
            project_key,
            #[cfg(test)]
            slot_index,
        })
    }

    async fn cleanup_created_workspace_then_release(
        &self,
        task_id: &TaskId,
        source_repo: &Path,
        workspace_path: &Path,
        known_worktree_registered: Option<bool>,
    ) -> anyhow::Result<()> {
        let cleanup_result = cleanup_workspace_path_with_registration(
            source_repo,
            workspace_path,
            known_worktree_registered,
        )
        .await;
        self.release_workspace(task_id).await;
        cleanup_result
    }
}
