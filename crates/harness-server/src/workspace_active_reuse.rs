use super::*;

pub(crate) struct WorkspaceStateGuard<'a> {
    manager: &'a WorkspaceManager,
    task_id: TaskId,
    acquisition_id: String,
    in_progress_state: ActiveWorkspaceState,
    armed: bool,
}

pub(crate) struct WorkspaceExecutionGuard {
    manager: Arc<WorkspaceManager>,
    task_id: TaskId,
    acquisition_id: String,
    execution_id: String,
    armed: std::sync::atomic::AtomicBool,
}

impl WorkspaceManager {
    pub(crate) fn guard_workspace_creation<'a>(
        &'a self,
        task_id: &TaskId,
        acquisition_id: &str,
    ) -> Result<WorkspaceStateGuard<'a>, WorkspaceLifecycleError> {
        self.workspace_state_guard(task_id, acquisition_id, ActiveWorkspaceState::Creating)
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
            ActiveWorkspaceSnapshot::from(active.value())
        };
        let repository_lease = self
            .acquire_repository_write_lease_for_cleanup(&snapshot.source_repo)
            .await?;
        let mut loss = repository_lease
            .as_ref()
            .map(RepositoryWriteLease::loss_receiver);
        let cleanup = async {
            let active = self
                .active
                .get(task_id)
                .ok_or_else(|| anyhow::anyhow!("workspace disappeared before retry cleanup"))?;
            if active.acquisition_id != snapshot.acquisition_id
                || active.state != ActiveWorkspaceState::CleanupRequired
            {
                anyhow::bail!("workspace acquisition changed before retry cleanup");
            }
            drop(active);
            if let Some(hook) = before_remove_hook {
                match timeout(
                    Duration::from_secs(hook_timeout_secs),
                    run_hook(hook, &snapshot.workspace_path),
                )
                .await
                {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => tracing::warn!(
                        task_id = %task_id.0,
                        "before_remove hook failed during retry cleanup: {error}"
                    ),
                    Err(_) => tracing::warn!(
                        task_id = %task_id.0,
                        "before_remove hook timed out during retry cleanup"
                    ),
                }
            }
            self.remove_workspace_acquisition(task_id, &snapshot.acquisition_id)
                .await
        };
        tokio::pin!(cleanup);
        if let Some(receiver) = loss.as_mut() {
            tokio::select! {
                biased;
                _ = wait_for_repository_revocation(receiver) => {
                    anyhow::bail!("repository lease was lost during retry cleanup")
                }
                result = &mut cleanup => result?,
            }
        } else {
            cleanup.await?;
        }
        Ok(())
    }

    pub(crate) fn begin_workspace_preparation<'a>(
        &'a self,
        task_id: &TaskId,
        acquisition_id: &str,
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
            armed: true,
        })
    }

    fn workspace_state_guard<'a>(
        &'a self,
        task_id: &TaskId,
        acquisition_id: &str,
        state: ActiveWorkspaceState,
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

    pub(crate) fn mark_workspace_cleanup_required(&self, task_id: &TaskId, acquisition_id: &str) {
        if let Some(mut active) = self.active.get_mut(task_id) {
            if active.acquisition_id == acquisition_id {
                active.state = ActiveWorkspaceState::CleanupRequired;
                active._repository_write_lease = None;
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
        if let Some(mut active) = self.manager.active.get_mut(&self.task_id) {
            if active.acquisition_id == self.acquisition_id
                && active.state == self.in_progress_state
            {
                active.state = ActiveWorkspaceState::CleanupRequired;
                active._repository_write_lease = None;
            }
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
