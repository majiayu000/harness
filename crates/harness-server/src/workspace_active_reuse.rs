use super::*;

pub(crate) struct WorkspaceStateGuard<'a> {
    manager: &'a WorkspaceManager,
    task_id: TaskId,
    acquisition_id: String,
    in_progress_state: ActiveWorkspaceState,
    before_remove_hook: Option<String>,
    hook_timeout_secs: u64,
    armed: bool,
}

pub(crate) struct WorkspaceExecutionGuard {
    manager: Arc<WorkspaceManager>,
    task_id: TaskId,
    acquisition_id: String,
    execution_id: String,
    armed: std::sync::atomic::AtomicBool,
}

struct CancelledWorkspaceSetupCleanup {
    active: Arc<DashMap<TaskId, ActiveWorkspace>>,
    active_paths: Arc<DashMap<PathBuf, TaskId>>,
    released_paths: Arc<DashMap<TaskId, PathBuf>>,
    released_workspace_paths: Arc<DashMap<String, PathBuf>>,
    git_ops: Arc<tokio::sync::Mutex<()>>,
    cleanup_ops: Arc<DashMap<String, Arc<WorkspaceCleanupOperation>>>,
    lease_store: Option<Arc<WorkspaceLeaseStore>>,
    workflow_before_remove_hook: Option<String>,
    workflow_hook_timeout_secs: u64,
    manager_before_remove_hook: Option<String>,
    manager_hook_timeout_secs: u64,
}

pub(super) fn workspace_cleanup_operation(
    cleanup_ops: &DashMap<String, Arc<WorkspaceCleanupOperation>>,
    acquisition_id: &str,
) -> Arc<WorkspaceCleanupOperation> {
    cleanup_ops
        .entry(acquisition_id.to_string())
        .or_insert_with(|| Arc::new(WorkspaceCleanupOperation::new()))
        .clone()
}

fn remove_workspace_cleanup_operation(
    cleanup_ops: &DashMap<String, Arc<WorkspaceCleanupOperation>>,
    acquisition_id: &str,
    cleanup_operation: &Arc<WorkspaceCleanupOperation>,
) {
    cleanup_ops.remove_if(acquisition_id, |_, current| {
        Arc::ptr_eq(current, cleanup_operation)
    });
}

pub(super) fn cancelled_cleanup_retry_delay(attempt: u64) -> std::time::Duration {
    let exponent = attempt.saturating_sub(1).min(7) as u32;
    std::time::Duration::from_millis(250_u64.saturating_mul(1_u64 << exponent))
        .min(std::time::Duration::from_secs(30))
}

pub(super) fn retry_cleanup_target_is_current(
    current: Option<(&str, &ActiveWorkspaceState)>,
    expected_acquisition_id: &str,
) -> anyhow::Result<bool> {
    let Some((acquisition_id, state)) = current else {
        return Ok(false);
    };
    if acquisition_id != expected_acquisition_id || state != &ActiveWorkspaceState::CleanupRequired
    {
        anyhow::bail!("workspace acquisition changed before retry cleanup");
    }
    Ok(true)
}

impl CancelledWorkspaceSetupCleanup {
    async fn converge(self, task_id: TaskId, snapshot: ActiveWorkspaceSnapshot) {
        let cleanup_operation =
            workspace_cleanup_operation(&self.cleanup_ops, &snapshot.acquisition_id);
        let mut attempt = 1_u64;
        loop {
            let still_current = self.active.get(&task_id).is_some_and(|active| {
                active.acquisition_id == snapshot.acquisition_id
                    && active.state == ActiveWorkspaceState::CleanupRequired
            });
            if !still_current {
                remove_workspace_cleanup_operation(
                    &self.cleanup_ops,
                    &snapshot.acquisition_id,
                    &cleanup_operation,
                );
                return;
            }

            let cleanup_guard = cleanup_operation.lock.lock().await;
            let still_current = self.active.get(&task_id).is_some_and(|active| {
                active.acquisition_id == snapshot.acquisition_id
                    && active.state == ActiveWorkspaceState::CleanupRequired
            });
            if !still_current {
                drop(cleanup_guard);
                remove_workspace_cleanup_operation(
                    &self.cleanup_ops,
                    &snapshot.acquisition_id,
                    &cleanup_operation,
                );
                return;
            }

            let repository_lease = match self.lease_store.as_ref() {
                Some(store) => match store
                    .acquire_queued_repository_write_lease(&snapshot.project_key)
                    .await
                {
                    Ok(lease) => Some(lease),
                    Err(error) => {
                        tracing::error!(
                            task_id = %task_id.0,
                            acquisition_id = %snapshot.acquisition_id,
                            attempt,
                            "failed to reacquire the repository lease for cancelled workspace setup cleanup; retrying: {error}"
                        );
                        drop(cleanup_guard);
                        let retry_delay = cancelled_cleanup_retry_delay(attempt);
                        attempt = attempt.saturating_add(1);
                        tokio::time::sleep(retry_delay).await;
                        continue;
                    }
                },
                None => None,
            };
            let repository_lease_lost = repository_lease
                .as_ref()
                .map(RepositoryWriteLease::loss_receiver);
            let cleanup = async {
                let still_current = self.active.get(&task_id).is_some_and(|active| {
                    active.acquisition_id == snapshot.acquisition_id
                        && active.state == ActiveWorkspaceState::CleanupRequired
                });
                if !still_current {
                    return Ok(false);
                }
                if self.workflow_before_remove_hook.is_some()
                    && cleanup_operation.claim_workflow_hook()
                {
                    run_workspace_cleanup_hook(
                        &task_id,
                        "workflow before_remove",
                        self.workflow_before_remove_hook.as_deref(),
                        self.workflow_hook_timeout_secs,
                        &snapshot.workspace_path,
                    )
                    .await;
                }
                if self.manager_before_remove_hook.is_some()
                    && cleanup_operation.claim_manager_hook()
                {
                    run_workspace_cleanup_hook(
                        &task_id,
                        "workspace before_remove",
                        self.manager_before_remove_hook.as_deref(),
                        self.manager_hook_timeout_secs,
                        &snapshot.workspace_path,
                    )
                    .await;
                }
                let _git_ops = self.git_ops.lock().await;
                let still_current = self.active.get(&task_id).is_some_and(|active| {
                    active.acquisition_id == snapshot.acquisition_id
                        && active.state == ActiveWorkspaceState::CleanupRequired
                });
                if !still_current {
                    return Ok(false);
                }
                cleanup_workspace_path(&snapshot.source_repo, &snapshot.workspace_path).await?;
                if let Some(store) = self.lease_store.as_ref() {
                    store
                        .complete_owned_workspace(
                            &snapshot.project_key,
                            snapshot.slot_index,
                            &task_id,
                            &snapshot.owner_session,
                            snapshot.run_generation,
                            &snapshot.acquisition_id,
                        )
                        .await?;
                }
                Ok::<bool, anyhow::Error>(true)
            };
            let result = run_until_repository_lease_loss(repository_lease_lost, cleanup).await;
            drop(repository_lease);
            match result {
                Some(Ok(true)) => {
                    let removed = self
                        .active
                        .remove_if(&task_id, |_, active| {
                            active.acquisition_id == snapshot.acquisition_id
                                && active.state == ActiveWorkspaceState::CleanupRequired
                        })
                        .map(|(_, active)| active);
                    if let Some(entry) = removed {
                        self.active_paths
                            .remove_if(&entry.workspace_path, |_, owner| owner == &task_id);
                        self.released_paths.remove(&task_id);
                        self.released_workspace_paths.remove(&entry.workspace_key);
                    }
                    drop(cleanup_guard);
                    remove_workspace_cleanup_operation(
                        &self.cleanup_ops,
                        &snapshot.acquisition_id,
                        &cleanup_operation,
                    );
                    return;
                }
                Some(Ok(false)) => {
                    drop(cleanup_guard);
                    remove_workspace_cleanup_operation(
                        &self.cleanup_ops,
                        &snapshot.acquisition_id,
                        &cleanup_operation,
                    );
                    return;
                }
                Some(Err(error)) => tracing::error!(
                    task_id = %task_id.0,
                    acquisition_id = %snapshot.acquisition_id,
                    attempt,
                    "failed to converge cancelled workspace setup cleanup; retrying: {error}"
                ),
                None => tracing::error!(
                    task_id = %task_id.0,
                    acquisition_id = %snapshot.acquisition_id,
                    attempt,
                    "repository lease was lost during cancelled workspace setup cleanup; retrying"
                ),
            }
            drop(cleanup_guard);
            let retry_delay = cancelled_cleanup_retry_delay(attempt);
            attempt = attempt.saturating_add(1);
            tokio::time::sleep(retry_delay).await;
        }
    }
}

impl WorkspaceManager {
    pub(crate) fn guard_workspace_creation<'a>(
        &'a self,
        task_id: &TaskId,
        acquisition_id: &str,
        before_remove_hook: Option<String>,
        hook_timeout_secs: u64,
    ) -> Result<WorkspaceStateGuard<'a>, WorkspaceLifecycleError> {
        self.workspace_state_guard(
            task_id,
            acquisition_id,
            ActiveWorkspaceState::Creating,
            before_remove_hook,
            hook_timeout_secs,
        )
    }

    pub(crate) async fn run_workspace_cleanup_hooks_once(
        &self,
        cleanup_operation: Option<&WorkspaceCleanupOperation>,
        task_id: &TaskId,
        workflow_hook: Option<&str>,
        workflow_hook_timeout_secs: u64,
        workspace_path: &Path,
    ) {
        if workflow_hook.is_some()
            && cleanup_operation.is_none_or(WorkspaceCleanupOperation::claim_workflow_hook)
        {
            run_workspace_cleanup_hook(
                task_id,
                "workflow before_remove",
                workflow_hook,
                workflow_hook_timeout_secs,
                workspace_path,
            )
            .await;
        }
        if self.config.before_remove_hook.is_some()
            && cleanup_operation.is_none_or(WorkspaceCleanupOperation::claim_manager_hook)
        {
            run_workspace_cleanup_hook(
                task_id,
                "workspace before_remove",
                self.config.before_remove_hook.as_deref(),
                self.config.hook_timeout_secs,
                workspace_path,
            )
            .await;
        }
    }

    pub(crate) async fn cleanup_required_workspace_for_retry(
        &self,
        task_id: &TaskId,
        before_remove_hook: Option<&str>,
        hook_timeout_secs: u64,
    ) -> anyhow::Result<()> {
        let snapshot = {
            let Some(mut active) = self.active.get_mut(task_id) else {
                return Ok(());
            };
            if active.state != ActiveWorkspaceState::CleanupRequired {
                return Ok(());
            }
            active._repository_write_lease = None;
            active._pool_permit = None;
            ActiveWorkspaceSnapshot::from(active.value())
        };
        let cleanup_operation =
            workspace_cleanup_operation(&self.cleanup_ops, &snapshot.acquisition_id);
        let cleanup_guard = cleanup_operation.lock.lock().await;
        let target_is_current = {
            let active = self.active.get(task_id);
            retry_cleanup_target_is_current(
                active
                    .as_ref()
                    .map(|active| (active.acquisition_id.as_str(), &active.state)),
                &snapshot.acquisition_id,
            )
        };
        match target_is_current {
            Ok(true) => {}
            Ok(false) => {
                drop(cleanup_guard);
                remove_workspace_cleanup_operation(
                    &self.cleanup_ops,
                    &snapshot.acquisition_id,
                    &cleanup_operation,
                );
                return Ok(());
            }
            Err(error) => {
                drop(cleanup_guard);
                remove_workspace_cleanup_operation(
                    &self.cleanup_ops,
                    &snapshot.acquisition_id,
                    &cleanup_operation,
                );
                return Err(error);
            }
        }
        let repository_lease = self
            .acquire_repository_write_lease_for_cleanup(&snapshot.source_repo)
            .await?;
        let mut loss = repository_lease
            .as_ref()
            .map(RepositoryWriteLease::loss_receiver);
        let cleanup = async {
            let active = self.active.get(task_id);
            if !retry_cleanup_target_is_current(
                active
                    .as_ref()
                    .map(|active| (active.acquisition_id.as_str(), &active.state)),
                &snapshot.acquisition_id,
            )? {
                return Ok(());
            }
            drop(active);
            self.run_workspace_cleanup_hooks_once(
                Some(&cleanup_operation),
                task_id,
                before_remove_hook,
                hook_timeout_secs,
                &snapshot.workspace_path,
            )
            .await;
            self.remove_workspace_acquisition_without_hook(task_id, &snapshot.acquisition_id)
                .await
        };
        tokio::pin!(cleanup);
        let result = if let Some(receiver) = loss.as_mut() {
            tokio::select! {
                biased;
                _ = wait_for_repository_revocation(receiver) => {
                    Err(anyhow::anyhow!("repository lease was lost during retry cleanup"))
                }
                result = &mut cleanup => result,
            }
        } else {
            cleanup.await
        };
        drop(repository_lease);
        drop(cleanup_guard);
        if result.is_ok() {
            remove_workspace_cleanup_operation(
                &self.cleanup_ops,
                &snapshot.acquisition_id,
                &cleanup_operation,
            );
        }
        result
    }

    pub(crate) fn begin_workspace_preparation<'a>(
        &'a self,
        task_id: &TaskId,
        acquisition_id: &str,
        before_remove_hook: Option<String>,
        hook_timeout_secs: u64,
    ) -> anyhow::Result<WorkspaceStateGuard<'a>> {
        let mut active = self
            .active
            .get_mut(task_id)
            .ok_or_else(|| anyhow::anyhow!("workspace disappeared before preparation"))?;
        if active.acquisition_id != acquisition_id {
            anyhow::bail!("workspace acquisition changed before preparation");
        }
        if active.state != ActiveWorkspaceState::Ready {
            anyhow::bail!("workspace requires cleanup before preparation can continue");
        }
        active.state = ActiveWorkspaceState::Preparing;
        Ok(WorkspaceStateGuard {
            manager: self,
            task_id: task_id.clone(),
            acquisition_id: acquisition_id.to_string(),
            in_progress_state: ActiveWorkspaceState::Preparing,
            before_remove_hook,
            hook_timeout_secs,
            armed: true,
        })
    }

    fn workspace_state_guard<'a>(
        &'a self,
        task_id: &TaskId,
        acquisition_id: &str,
        state: ActiveWorkspaceState,
        before_remove_hook: Option<String>,
        hook_timeout_secs: u64,
    ) -> Result<WorkspaceStateGuard<'a>, WorkspaceLifecycleError> {
        let active =
            self.active
                .get(task_id)
                .ok_or_else(|| WorkspaceLifecycleError::CreateFailed {
                    message: "workspace disappeared while installing state guard".to_string(),
                })?;
        if active.acquisition_id != acquisition_id || active.state != state {
            return Err(WorkspaceLifecycleError::CreateFailed {
                message: "workspace acquisition changed while installing state guard".to_string(),
            });
        }
        Ok(WorkspaceStateGuard {
            manager: self,
            task_id: task_id.clone(),
            acquisition_id: acquisition_id.to_string(),
            in_progress_state: state,
            before_remove_hook,
            hook_timeout_secs,
            armed: true,
        })
    }

    pub(crate) fn claim_workspace_execution(
        self: &Arc<Self>,
        task_id: &TaskId,
        acquisition_id: &str,
    ) -> anyhow::Result<WorkspaceExecutionGuard> {
        let execution_id = SessionId::new().to_string();
        let mut active = self
            .active
            .get_mut(task_id)
            .ok_or_else(|| anyhow::anyhow!("workspace disappeared before execution"))?;
        if active.acquisition_id != acquisition_id || active.state != ActiveWorkspaceState::Ready {
            anyhow::bail!("workspace acquisition is not ready for execution");
        }
        active.state = ActiveWorkspaceState::Running(execution_id.clone());
        Ok(WorkspaceExecutionGuard {
            manager: Arc::clone(self),
            task_id: task_id.clone(),
            acquisition_id: acquisition_id.to_string(),
            execution_id,
            armed: std::sync::atomic::AtomicBool::new(true),
        })
    }

    pub(crate) fn begin_workspace_finalization(
        &self,
        task_id: &TaskId,
        acquisition_id: &str,
        execution_id: &str,
    ) -> anyhow::Result<()> {
        let mut active = self
            .active
            .get_mut(task_id)
            .ok_or_else(|| anyhow::anyhow!("workspace disappeared before finalization"))?;
        if active.acquisition_id != acquisition_id
            || !matches!(&active.state, ActiveWorkspaceState::Running(id) if id == execution_id)
        {
            anyhow::bail!("workspace execution ownership changed before finalization");
        }
        active.state = ActiveWorkspaceState::Finalizing(execution_id.to_string());
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn mark_workspace_cleanup_required(&self, task_id: &TaskId, acquisition_id: &str) {
        if let Some(mut active) = self.active.get_mut(task_id) {
            if active.acquisition_id == acquisition_id {
                active.state = ActiveWorkspaceState::CleanupRequired;
                active._repository_write_lease = None;
                active._pool_permit = None;
            }
        }
    }

    pub(super) fn prepare_active_repository_lease_resolution(
        &self,
        task_id: &TaskId,
        run_generation: u32,
    ) -> Result<(), WorkspaceLifecycleError> {
        let Some(mut active) = self.active.get_mut(task_id) else {
            return Ok(());
        };
        self.validate_active_workspace_owner(&active, run_generation)?;
        if active.state != ActiveWorkspaceState::Ready {
            return Err(WorkspaceLifecycleError::CreateFailed {
                message: format!("workspace for task {} is already in use", task_id.0),
            });
        }
        active._repository_write_lease = None;
        Ok(())
    }

    pub(super) async fn try_reuse_active_workspace(
        &self,
        task_id: &TaskId,
        run_generation: u32,
        desired_capacity: usize,
        repository_write_lease: &mut Option<RepositoryWriteLease>,
    ) -> Result<Option<WorkspaceLease>, WorkspaceLifecycleError> {
        let Some(active) = self.active.get(task_id) else {
            return Ok(None);
        };
        self.validate_active_workspace_owner(&active, run_generation)?;
        if active.state != ActiveWorkspaceState::Ready {
            return Err(WorkspaceLifecycleError::CreateFailed {
                message: format!(
                    "workspace for task {} requires cleanup before it can be reused",
                    task_id.0
                ),
            });
        }
        let acquisition_id = active.acquisition_id.clone();
        if active.slot_index >= desired_capacity as u32 {
            drop(active);
            if let Some(mut active) = self.active.get_mut(task_id) {
                if active.acquisition_id == acquisition_id {
                    active.state = ActiveWorkspaceState::CleanupRequired;
                    active._repository_write_lease = None;
                    active._pool_permit = None;
                }
            }
            return Err(WorkspaceLifecycleError::CreateFailed {
                message: format!(
                    "workspace for task {} uses a slot outside the reduced capacity",
                    task_id.0
                ),
            });
        }
        let workspace_path = active.workspace_path.clone();
        drop(active);

        if !workspace_path.exists() {
            let removed = self
                .active
                .remove_if(task_id, |_, active| {
                    active.workspace_path == workspace_path
                        && active.run_generation == run_generation
                        && active.owner_session == self.owner_session
                        && active.acquisition_id == acquisition_id
                })
                .map(|(_, active)| active);
            if let Some(entry) = removed {
                self.release_active_path(task_id, &entry.workspace_path);
                self.released_paths.remove(task_id);
                self.released_workspace_paths.remove(&entry.workspace_key);
                self.release_persisted_lease(task_id, &entry)
                    .await
                    .map_err(|error| WorkspaceLifecycleError::CreateFailed {
                        message: format!("failed to release missing workspace: {error}"),
                    })?;
            }
            return Ok(None);
        }

        let mut active =
            self.active
                .get_mut(task_id)
                .ok_or_else(|| WorkspaceLifecycleError::CreateFailed {
                    message: format!("workspace for task {} disappeared during reuse", task_id.0),
                })?;
        self.validate_active_workspace_owner(&active, run_generation)?;
        Self::attach_repository_lease(&mut active, repository_write_lease);
        Ok(Some(WorkspaceLease {
            workspace_path: active.workspace_path.clone(),
            acquisition_id: active.acquisition_id.clone(),
            repository_lease_lost: active
                ._repository_write_lease
                .as_ref()
                .map(RepositoryWriteLease::loss_receiver),
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
        }))
    }

    fn validate_active_workspace_owner(
        &self,
        active: &ActiveWorkspace,
        run_generation: u32,
    ) -> Result<(), WorkspaceLifecycleError> {
        if active.run_generation == run_generation && active.owner_session == self.owner_session {
            return Ok(());
        }
        Err(WorkspaceLifecycleError::LiveForeignOwner {
            message: format!(
                "WorktreeCollision: workspace path {:?} already owned by another harness session; manual resolution required",
                active.workspace_path
            ),
        })
    }
}

async fn wait_for_repository_revocation(
    receiver: &mut tokio::sync::watch::Receiver<RepositoryLeaseState>,
) {
    loop {
        if matches!(
            *receiver.borrow(),
            RepositoryLeaseState::Revoking | RepositoryLeaseState::Lost
        ) {
            return;
        }
        if receiver.changed().await.is_err() {
            return;
        }
    }
}

impl WorkspaceStateGuard<'_> {
    pub(crate) fn complete(mut self) -> Result<(), WorkspaceLifecycleError> {
        let mut active = self.manager.active.get_mut(&self.task_id).ok_or_else(|| {
            WorkspaceLifecycleError::CreateFailed {
                message: "workspace disappeared while completing guarded operation".to_string(),
            }
        })?;
        if active.acquisition_id != self.acquisition_id || active.state != self.in_progress_state {
            return Err(WorkspaceLifecycleError::CreateFailed {
                message: "workspace acquisition changed while completing guarded operation"
                    .to_string(),
            });
        }
        active.state = ActiveWorkspaceState::Ready;
        self.armed = false;
        Ok(())
    }
}

impl Drop for WorkspaceStateGuard<'_> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let snapshot = if let Some(mut active) = self.manager.active.get_mut(&self.task_id) {
            if active.acquisition_id == self.acquisition_id
                && active.state == self.in_progress_state
            {
                active.state = ActiveWorkspaceState::CleanupRequired;
                active._repository_write_lease = None;
                active._pool_permit = None;
                Some(ActiveWorkspaceSnapshot::from(active.value()))
            } else {
                None
            }
        } else {
            None
        };
        let Some(snapshot) = snapshot else {
            return;
        };
        let cleanup = CancelledWorkspaceSetupCleanup {
            active: Arc::clone(&self.manager.active),
            active_paths: Arc::clone(&self.manager.active_paths),
            released_paths: Arc::clone(&self.manager.released_paths),
            released_workspace_paths: Arc::clone(&self.manager.released_workspace_paths),
            git_ops: Arc::clone(&self.manager.git_ops),
            cleanup_ops: Arc::clone(&self.manager.cleanup_ops),
            lease_store: self.manager.lease_store.clone(),
            workflow_before_remove_hook: self.before_remove_hook.take(),
            workflow_hook_timeout_secs: self.hook_timeout_secs,
            manager_before_remove_hook: self.manager.config.before_remove_hook.clone(),
            manager_hook_timeout_secs: self.manager.config.hook_timeout_secs,
        };
        let task_id = self.task_id.clone();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(cleanup.converge(task_id, snapshot));
        } else {
            tracing::error!(
                task_id = %self.task_id.0,
                "cancelled workspace setup requires cleanup but no Tokio runtime is available"
            );
        }
    }
}

impl WorkspaceExecutionGuard {
    pub(crate) fn execution_id(&self) -> &str {
        &self.execution_id
    }

    pub(crate) fn complete(&self) {
        self.armed
            .store(false, std::sync::atomic::Ordering::Release);
    }
}

impl Drop for WorkspaceExecutionGuard {
    fn drop(&mut self) {
        if !self.armed.load(std::sync::atomic::Ordering::Acquire) {
            return;
        }
        let cleanup_required = if let Some(mut active) = self.manager.active.get_mut(&self.task_id)
        {
            if active.acquisition_id == self.acquisition_id
                && matches!(
                    &active.state,
                    ActiveWorkspaceState::Running(id) | ActiveWorkspaceState::Finalizing(id)
                        if id == &self.execution_id
                )
            {
                active.state = ActiveWorkspaceState::CleanupRequired;
                active._repository_write_lease = None;
                active._pool_permit = None;
                true
            } else {
                false
            }
        } else {
            false
        };
        if !cleanup_required {
            return;
        }
        let manager = Arc::clone(&self.manager);
        let task_id = self.task_id.clone();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                if let Err(error) = manager
                    .cleanup_required_workspace_for_retry(&task_id, None, 0)
                    .await
                {
                    tracing::error!(
                        task_id = %task_id.0,
                        "failed to converge cancelled workspace execution cleanup: {error}"
                    );
                }
            });
        } else {
            tracing::error!(
                task_id = %self.task_id.0,
                "cancelled workspace execution requires cleanup but no Tokio runtime is available"
            );
        }
    }
}
