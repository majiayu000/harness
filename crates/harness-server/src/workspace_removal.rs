use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WorkspaceRetryCleanupOutcome {
    Removed,
    DeferredActive,
}

pub(super) struct PersistedWorkspaceAcquisitionGuard {
    store: Option<Arc<WorkspaceLeaseStore>>,
    project_key: String,
    slot_index: u32,
    task_id: TaskId,
    owner_session: String,
    run_generation: u32,
    acquisition_id: String,
    acquisition_task: Option<tokio::task::JoinHandle<anyhow::Result<bool>>>,
    armed: bool,
}

impl PersistedWorkspaceAcquisitionGuard {
    async fn resolve(&mut self) -> anyhow::Result<bool> {
        let Some(task) = self.acquisition_task.as_mut() else {
            return Ok(true);
        };
        let acquired = task
            .await
            .map_err(|error| anyhow::anyhow!("workspace acquisition task failed: {error}"))??;
        self.acquisition_task = None;
        if !acquired {
            self.armed = false;
        }
        Ok(acquired)
    }

    pub(super) async fn rollback(&mut self) -> anyhow::Result<()> {
        if let Some(store) = self.store.as_ref() {
            let released = store
                .release_owned_slot(
                    &self.project_key,
                    self.slot_index,
                    &self.task_id,
                    &self.owner_session,
                    self.run_generation,
                    &self.acquisition_id,
                )
                .await?;
            if !released {
                anyhow::bail!("workspace acquisition changed before rollback");
            }
        }
        self.armed = false;
        Ok(())
    }

    pub(super) fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for PersistedWorkspaceAcquisitionGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let Some(store) = self.store.clone() else {
            return;
        };
        let project_key = self.project_key.clone();
        let slot_index = self.slot_index;
        let task_id = self.task_id.clone();
        let owner_session = self.owner_session.clone();
        let run_generation = self.run_generation;
        let acquisition_id = self.acquisition_id.clone();
        let acquisition_task = self.acquisition_task.take();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                let acquired = match acquisition_task {
                    Some(task) => match task.await {
                        Ok(Ok(acquired)) => acquired,
                        Ok(Err(error)) => {
                            tracing::error!(
                                task_id = %task_id.0,
                                "cancelled workspace acquisition returned an error; attempting exact rollback in case the commit succeeded: {error}"
                            );
                            true
                        }
                        Err(error) => {
                            tracing::error!(
                                task_id = %task_id.0,
                                "cancelled workspace acquisition task failed; attempting exact rollback in case the commit succeeded: {error}"
                            );
                            true
                        }
                    },
                    None => true,
                };
                if !acquired {
                    return;
                }
                let mut attempt = 1_u64;
                loop {
                    match store
                        .release_owned_slot(
                            &project_key,
                            slot_index,
                            &task_id,
                            &owner_session,
                            run_generation,
                            &acquisition_id,
                        )
                        .await
                    {
                        Ok(true) => return,
                        Ok(false) => {
                            tracing::warn!(
                                task_id = %task_id.0,
                                acquisition_id = %acquisition_id,
                                "cancelled workspace acquisition was already released or replaced before exact rollback"
                            );
                            return;
                        }
                        Err(error) => {
                            tracing::error!(
                                task_id = %task_id.0,
                                acquisition_id = %acquisition_id,
                                attempt,
                                "failed to roll back cancelled workspace acquisition; retrying exact rollback: {error}"
                            );
                            attempt = attempt.saturating_add(1);
                            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                        }
                    }
                }
            });
        }
    }
}

struct WorkspaceRemovalGuard<'a> {
    manager: &'a WorkspaceManager,
    task_id: TaskId,
    snapshot: ActiveWorkspaceSnapshot,
    armed: bool,
}

impl<'a> WorkspaceRemovalGuard<'a> {
    fn new(
        manager: &'a WorkspaceManager,
        task_id: &TaskId,
        snapshot: ActiveWorkspaceSnapshot,
    ) -> Self {
        Self {
            manager,
            task_id: task_id.clone(),
            snapshot,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for WorkspaceRemovalGuard<'_> {
    fn drop(&mut self) {
        if !self.armed || self.snapshot.workspace_path.exists() {
            return;
        }
        let removed = self
            .manager
            .active
            .remove_if(&self.task_id, |_, active| {
                active.workspace_path == self.snapshot.workspace_path
                    && active.owner_session == self.snapshot.owner_session
                    && active.run_generation == self.snapshot.run_generation
                    && active.acquisition_id == self.snapshot.acquisition_id
            })
            .map(|(_, active)| active);
        if let Some(entry) = removed {
            self.manager
                .release_active_path(&self.task_id, &entry.workspace_path);
            self.manager.released_paths.remove(&self.task_id);
            self.manager
                .released_workspace_paths
                .remove(&entry.workspace_key);
        }
        let Some(store) = self.manager.lease_store.clone() else {
            return;
        };
        let task_id = self.task_id.clone();
        let project_key = self.snapshot.project_key.clone();
        let slot_index = self.snapshot.slot_index;
        let owner_session = self.snapshot.owner_session.clone();
        let run_generation = self.snapshot.run_generation;
        let acquisition_id = self.snapshot.acquisition_id.clone();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                let mut attempt = 1_u64;
                loop {
                    match store
                        .release_owned_slot(
                            &project_key,
                            slot_index,
                            &task_id,
                            &owner_session,
                            run_generation,
                            &acquisition_id,
                        )
                        .await
                    {
                        Ok(true) => return,
                        Ok(false) => {
                            tracing::warn!(
                                task_id = %task_id.0,
                                acquisition_id = %acquisition_id,
                                "cancelled workspace removal lease was already released or replaced"
                            );
                            return;
                        }
                        Err(error) => {
                            tracing::error!(
                                task_id = %task_id.0,
                                acquisition_id = %acquisition_id,
                                attempt,
                                "failed to release cancelled workspace removal lease; retrying exact release: {error}"
                            );
                            attempt = attempt.saturating_add(1);
                            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                        }
                    }
                }
            });
        }
    }
}

impl WorkspaceManager {
    pub(super) async fn acquire_persisted_workspace_guard(
        &self,
        record: WorkspaceLeaseRecord,
        persist_cleanup_target: bool,
    ) -> anyhow::Result<Option<PersistedWorkspaceAcquisitionGuard>> {
        let store = self.lease_store.clone();
        let acquisition_id = record
            .acquisition_id
            .clone()
            .ok_or_else(|| anyhow::anyhow!("workspace acquisition ID is missing"))?;
        let acquisition_task = store.as_ref().map(|store| {
            let store = store.clone();
            let record = record.clone();
            tokio::spawn(async move {
                store
                    .try_acquire_lease_with_cleanup_target(&record, persist_cleanup_target)
                    .await
            })
        });
        let mut guard = PersistedWorkspaceAcquisitionGuard {
            store,
            project_key: record.project_key,
            slot_index: record.slot_index,
            task_id: record.task_id,
            owner_session: record.owner_session,
            run_generation: record.run_generation,
            acquisition_id,
            acquisition_task,
            armed: self.lease_store.is_some(),
        };
        guard
            .resolve()
            .await
            .map(|acquired| acquired.then_some(guard))
    }

    pub(super) async fn release_workspace_after_create_failure(
        &self,
        task_id: &TaskId,
        acquisition_id: &str,
    ) -> Result<(), WorkspaceLifecycleError> {
        self.release_workspace_acquisition(task_id, acquisition_id)
            .await
            .map_err(|error| WorkspaceLifecycleError::ReconcileFailed {
                message: format!("failed to release workspace acquisition: {error}"),
            })
    }

    pub(super) async fn cleanup_created_workspace_then_release(
        &self,
        task_id: &TaskId,
        source_repo: &Path,
        workspace_path: &Path,
        known_worktree_registered: Option<bool>,
        acquisition_id: &str,
    ) -> anyhow::Result<()> {
        let cleanup_result = cleanup_workspace_path_with_registration(
            source_repo,
            workspace_path,
            known_worktree_registered,
        )
        .await;
        self.release_workspace_acquisition(task_id, acquisition_id)
            .await?;
        cleanup_result
    }

    /// Remove a workspace while retaining its lease and path reservation until
    /// physical cleanup succeeds. Cancellation converges metadata when the path
    /// has already disappeared and leaves the durable cleanup target for retry.
    pub async fn remove_workspace(&self, task_id: &TaskId) -> anyhow::Result<()> {
        self.remove_workspace_inner(task_id, None).await
    }

    pub(crate) async fn remove_workspace_acquisition(
        &self,
        task_id: &TaskId,
        acquisition_id: &str,
    ) -> anyhow::Result<()> {
        self.remove_workspace_inner(task_id, Some(acquisition_id))
            .await
    }

    async fn remove_workspace_inner(
        &self,
        task_id: &TaskId,
        expected_acquisition_id: Option<&str>,
    ) -> anyhow::Result<()> {
        let snapshot = match self
            .active
            .get(task_id)
            .filter(|entry| {
                expected_acquisition_id.is_none_or(|expected| entry.acquisition_id == expected)
            })
            .map(|entry| ActiveWorkspaceSnapshot::from(entry.value()))
        {
            Some(snapshot) => snapshot,
            None => return Ok(()),
        };
        let mut guard = WorkspaceRemovalGuard::new(self, task_id, snapshot.clone());

        if let Some(hook) = &self.config.before_remove_hook {
            let timeout_secs = self.config.hook_timeout_secs;
            match timeout(
                Duration::from_secs(timeout_secs),
                run_hook(hook, &snapshot.workspace_path),
            )
            .await
            {
                Ok(Ok(())) => {}
                Ok(Err(error)) => tracing::warn!("before_remove_hook failed: {error}"),
                Err(_) => tracing::warn!("before_remove_hook timed out after {timeout_secs}s"),
            }
        }

        let _git_ops_guard = self.git_ops.lock().await;
        let still_current = self.active.get(task_id).is_some_and(|active| {
            active.workspace_path == snapshot.workspace_path
                && active.owner_session == snapshot.owner_session
                && active.run_generation == snapshot.run_generation
                && active.acquisition_id == snapshot.acquisition_id
        });
        if !still_current {
            return Ok(());
        }

        cleanup_workspace_path(&snapshot.source_repo, &snapshot.workspace_path).await?;
        let removed = self
            .active
            .remove_if(task_id, |_, active| {
                active.workspace_path == snapshot.workspace_path
                    && active.owner_session == snapshot.owner_session
                    && active.run_generation == snapshot.run_generation
                    && active.acquisition_id == snapshot.acquisition_id
            })
            .map(|(_, active)| active);
        if let Some(entry) = removed {
            self.release_active_path(task_id, &entry.workspace_path);
            self.released_paths.remove(task_id);
            self.released_workspace_paths.remove(&entry.workspace_key);
            if let Some(store) = self.lease_store.as_ref() {
                store
                    .complete_owned_workspace(
                        &entry.project_key,
                        entry.slot_index,
                        task_id,
                        &entry.owner_session,
                        entry.run_generation,
                        &entry.acquisition_id,
                    )
                    .await?;
            }
        } else {
            if let Some(store) = self.lease_store.as_ref() {
                store
                    .release_owned_slot(
                        &snapshot.project_key,
                        snapshot.slot_index,
                        task_id,
                        &snapshot.owner_session,
                        snapshot.run_generation,
                        &snapshot.acquisition_id,
                    )
                    .await?;
            }
            self.released_paths.remove(task_id);
            self.released_workspace_paths
                .remove(&snapshot.workspace_key);
        }
        guard.disarm();
        Ok(())
    }

    pub(crate) async fn cleanup_workspace_for_retry(
        &self,
        task_id: &TaskId,
        source_repo: &Path,
        workspace_path: Option<&Path>,
    ) -> anyhow::Result<WorkspaceRetryCleanupOutcome> {
        self.cleanup_workspace_for_retry_inner(task_id, source_repo, workspace_path, None)
            .await
    }

    pub(crate) async fn cleanup_workspace_acquisition_for_retry(
        &self,
        task_id: &TaskId,
        source_repo: &Path,
        workspace_path: &Path,
        acquisition_id: &str,
    ) -> anyhow::Result<WorkspaceRetryCleanupOutcome> {
        self.cleanup_workspace_for_retry_inner(
            task_id,
            source_repo,
            Some(workspace_path),
            Some(acquisition_id),
        )
        .await
    }

    pub(crate) fn active_workspace_acquisition_id(
        &self,
        task_id: &TaskId,
        workspace_path: &Path,
    ) -> Option<String> {
        self.active.get(task_id).and_then(|active| {
            (active.workspace_path == workspace_path).then(|| active.acquisition_id.clone())
        })
    }

    async fn cleanup_workspace_for_retry_inner(
        &self,
        task_id: &TaskId,
        source_repo: &Path,
        workspace_path: Option<&Path>,
        expected_acquisition_id: Option<&str>,
    ) -> anyhow::Result<WorkspaceRetryCleanupOutcome> {
        let _git_ops_guard = self.git_ops.lock().await;
        let target = workspace_path
            .map(Path::to_path_buf)
            .or_else(|| {
                self.active
                    .get(task_id)
                    .map(|entry| entry.workspace_path.clone())
            })
            .unwrap_or_else(|| self.config.root.join(sanitize_task_id(&task_id.0)));
        if let Some(owner_task) = self.active_paths.get(&target) {
            if owner_task.value() != task_id {
                tracing::info!(
                    task_id = %task_id.0,
                    owner_task_id = %owner_task.value().0,
                    workspace_path = ?target,
                    "deferring workspace retry cleanup reserved by another active task"
                );
                return Ok(WorkspaceRetryCleanupOutcome::DeferredActive);
            }
        }

        let snapshot = self.active.get(task_id).and_then(|active| {
            (active.workspace_path == target
                && expected_acquisition_id.is_none_or(|expected| active.acquisition_id == expected)
                && crate::workspace_pool::project_limit_key(&active.source_repo)
                    == crate::workspace_pool::project_limit_key(source_repo))
            .then(|| ActiveWorkspaceSnapshot::from(active.value()))
        });
        let mut cancellation_guard = snapshot
            .clone()
            .map(|snapshot| WorkspaceRemovalGuard::new(self, task_id, snapshot));
        if expected_acquisition_id.is_some()
            && self.active.get(task_id).is_some_and(|active| {
                active.workspace_path == target
                    && expected_acquisition_id
                        .is_some_and(|expected| active.acquisition_id != expected)
            })
        {
            return Ok(WorkspaceRetryCleanupOutcome::DeferredActive);
        }
        cleanup_workspace_path(source_repo, &target).await?;

        let removed = snapshot.as_ref().and_then(|snapshot| {
            self.active
                .remove_if(task_id, |_, active| {
                    active.workspace_path == snapshot.workspace_path
                        && active.owner_session == snapshot.owner_session
                        && active.run_generation == snapshot.run_generation
                        && active.acquisition_id == snapshot.acquisition_id
                })
                .map(|(_, active)| active)
        });
        if let Some(entry) = removed.as_ref() {
            self.release_active_path(task_id, &entry.workspace_path);
            self.release_persisted_lease(task_id, entry).await?;
        }
        self.released_paths
            .remove_if(task_id, |_, path| *path == target);
        if let Some(entry) = removed.as_ref() {
            self.released_workspace_paths
                .remove_if(&entry.workspace_key, |_, path| *path == target);
        }
        if let Some(guard) = cancellation_guard.as_mut() {
            guard.disarm();
        }
        Ok(WorkspaceRetryCleanupOutcome::Removed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stale_removal_guard_cannot_remove_replacement_acquisition() {
        let dir = tempfile::tempdir().expect("tempdir");
        let manager = WorkspaceManager::new(WorkspaceConfig {
            root: dir.path().join("workspaces"),
            ..Default::default()
        })
        .expect("manager");
        let task_id = TaskId("guard-task".to_string());
        let active = |acquisition_id: &str| ActiveWorkspace {
            workspace_path: dir.path().join("workspaces/guard-task"),
            source_repo: dir.path().join("repo"),
            repo: None,
            runtime_workflow_id: None,
            workspace_key: "guard-workspace".to_string(),
            project_key: "guard-project".to_string(),
            slot_index: 0,
            branch: "harness/guard-task".to_string(),
            created_at: SystemTime::now(),
            owner_session: manager.owner_session.clone(),
            run_generation: 1,
            acquisition_id: acquisition_id.to_string(),
            state: ActiveWorkspaceState::Ready,
            _pool_permit: None,
            _repository_write_lease: None,
        };
        manager
            .active
            .insert(task_id.clone(), active("acquisition-a"));
        let snapshot = ActiveWorkspaceSnapshot::from(
            manager.active.get(&task_id).expect("acquisition A").value(),
        );
        let guard = WorkspaceRemovalGuard::new(&manager, &task_id, snapshot);
        manager
            .active
            .insert(task_id.clone(), active("acquisition-b"));

        drop(guard);

        assert_eq!(
            manager
                .active
                .get(&task_id)
                .expect("replacement acquisition")
                .acquisition_id,
            "acquisition-b"
        );
    }
}
