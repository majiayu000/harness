use super::*;

pub(crate) struct RuntimeWorkspaceCleanupTarget {
    pub(crate) task_id: TaskId,
    pub(crate) workspace_path: PathBuf,
    persisted_target: Option<crate::workspace_lease_store::WorkspaceCleanupTargetRecord>,
}

pub(crate) enum RepositoryWriteLeaseAttempt {
    NotRequired,
    Acquired(RepositoryWriteLease),
    Contended,
}

impl WorkspaceManager {
    pub(crate) async fn acquire_repository_lease_from_current_config(
        &self,
        source_repo: &Path,
    ) -> anyhow::Result<(Option<RepositoryWriteLease>, usize)> {
        loop {
            let capacity = self.resolve_workspace_capacity(source_repo).await?;
            let Some(mode) = self.pool.repository_lease_mode_for_capacity(capacity) else {
                return Ok((None, capacity));
            };
            let lease = self.acquire_repository_lease(source_repo, mode).await?;
            let current_capacity = self.resolve_workspace_capacity(source_repo).await?;
            if mode == RepositoryLeaseMode::Exclusive || current_capacity > 1 {
                return Ok((Some(lease), current_capacity));
            }
            drop(lease);
        }
    }

    pub(crate) async fn resolve_workspace_capacity(
        &self,
        source_repo: &Path,
    ) -> anyhow::Result<usize> {
        let Some(source) = self.capacity_source.as_ref() else {
            return Ok(self.pool.capacity_for(source_repo));
        };
        Ok(
            crate::http::builders::workspace_pool_config::build_workspace_pool_config(
                source.server.as_ref(),
                source.project_registry.as_ref(),
            )
            .await?
            .capacity_for(source_repo),
        )
    }

    pub(crate) async fn acquire_repository_write_lease(
        &self,
        source_repo: &Path,
    ) -> anyhow::Result<RepositoryWriteLease> {
        self.acquire_repository_lease(source_repo, RepositoryLeaseMode::Exclusive)
            .await
    }

    pub(crate) async fn acquire_repository_lease_for_runtime(
        &self,
        source_repo: &Path,
        single_writer: bool,
    ) -> anyhow::Result<Option<RepositoryWriteLease>> {
        let Some(mode) = self
            .pool
            .repository_lease_mode_for_capacity(if single_writer { 1 } else { 2 })
        else {
            return Ok(None);
        };
        self.acquire_repository_lease(source_repo, mode)
            .await
            .map(Some)
    }

    async fn acquire_repository_lease(
        &self,
        source_repo: &Path,
        mode: RepositoryLeaseMode,
    ) -> anyhow::Result<RepositoryWriteLease> {
        let store = self.lease_store.as_ref().ok_or_else(|| {
            anyhow::anyhow!("repository execution requires the PostgreSQL workspace lease store")
        })?;
        let project_key = crate::workspace_pool::project_limit_key(source_repo);
        let mut delay = Duration::from_millis(250);
        loop {
            let lease = match mode {
                RepositoryLeaseMode::Shared => {
                    store
                        .try_acquire_repository_shared_lease(&project_key)
                        .await?
                }
                RepositoryLeaseMode::Exclusive => {
                    store
                        .try_acquire_repository_write_lease(&project_key)
                        .await?
                }
            };
            if let Some(lease) = lease {
                return Ok(lease);
            }
            tracing::debug!(
                project_key = %project_key,
                ?mode,
                "workspace pool waiting for the PostgreSQL repository lease"
            );
            tokio::time::sleep(delay).await;
            delay = std::cmp::min(delay * 2, Duration::from_secs(5));
        }
    }

    pub(crate) async fn acquire_repository_write_lease_for_cleanup(
        &self,
        source_repo: &Path,
    ) -> anyhow::Result<Option<RepositoryWriteLease>> {
        if self.pool.repository_lease_mode_for_capacity(1).is_none() {
            return Ok(None);
        }
        self.acquire_repository_write_lease(source_repo)
            .await
            .map(Some)
    }

    pub(crate) async fn try_acquire_repository_write_lease_for_reconciliation(
        &self,
        source_repo: &Path,
    ) -> anyhow::Result<RepositoryWriteLeaseAttempt> {
        if self.pool.repository_lease_mode_for_capacity(1).is_none() {
            return Ok(RepositoryWriteLeaseAttempt::NotRequired);
        }
        let store = self.lease_store.as_ref().ok_or_else(|| {
            anyhow::anyhow!(
                "repository reconciliation requires the PostgreSQL workspace lease store"
            )
        })?;
        let project_key = crate::workspace_pool::project_limit_key(source_repo);
        Ok(
            match store
                .try_acquire_repository_write_lease_now(&project_key)
                .await?
            {
                Some(lease) => RepositoryWriteLeaseAttempt::Acquired(lease),
                None => RepositoryWriteLeaseAttempt::Contended,
            },
        )
    }

    pub(crate) async fn workspace_targets_for_runtime_workflow(
        &self,
        runtime_workflow_id: &str,
    ) -> anyhow::Result<Vec<RuntimeWorkspaceCleanupTarget>> {
        let mut targets = if let Some(store) = self.lease_store.as_ref() {
            store
                .workspace_cleanup_targets_for_runtime_workflow(runtime_workflow_id)
                .await?
                .into_iter()
                .map(|record| RuntimeWorkspaceCleanupTarget {
                    task_id: record.task_id.clone(),
                    workspace_path: record.workspace_path.clone(),
                    persisted_target: Some(record),
                })
                .collect::<Vec<_>>()
        } else {
            self.active
                .iter()
                .filter(|entry| entry.runtime_workflow_id.as_deref() == Some(runtime_workflow_id))
                .map(|entry| RuntimeWorkspaceCleanupTarget {
                    task_id: entry.key().clone(),
                    workspace_path: entry.workspace_path.clone(),
                    persisted_target: None,
                })
                .collect::<Vec<_>>()
        };
        let mut seen_paths = HashSet::new();
        targets.retain(|target| seen_paths.insert(target.workspace_path.clone()));
        Ok(targets)
    }

    pub(crate) async fn runtime_workspace_cleanup_target_is_superseded(
        &self,
        target: &RuntimeWorkspaceCleanupTarget,
    ) -> anyhow::Result<bool> {
        let (Some(store), Some(record)) =
            (self.lease_store.as_ref(), target.persisted_target.as_ref())
        else {
            return Ok(false);
        };
        let current = store
            .current_workspace_lease_for_slot(&record.project_key, record.slot_index)
            .await?;
        Ok(current.is_some_and(|current| {
            current.workspace_path == record.workspace_path
                && current.runtime_workflow_id.as_deref()
                    != Some(record.runtime_workflow_id.as_str())
        }))
    }

    pub(crate) async fn release_runtime_workspace_cleanup_target(
        &self,
        target: &RuntimeWorkspaceCleanupTarget,
    ) -> anyhow::Result<()> {
        if let (Some(store), Some(record)) =
            (self.lease_store.as_ref(), target.persisted_target.as_ref())
        {
            store.complete_workspace_cleanup_target(record).await?;
        } else {
            self.release_workspace(&target.task_id).await;
        }
        Ok(())
    }

    pub(crate) async fn cleanup_reconciliation_workspace_path(
        &self,
        source_repo: &Path,
        workspace_path: &Path,
    ) -> anyhow::Result<bool> {
        let _repository_lease = self
            .acquire_repository_write_lease_for_cleanup(source_repo)
            .await?;
        let _git_ops = self.git_ops.lock().await;
        if self
            .active
            .iter()
            .any(|entry| entry.workspace_path == workspace_path)
        {
            return Ok(false);
        }
        if let Some(store) = self.lease_store.as_ref() {
            if store.leased_workspace_path(workspace_path).await?.is_some() {
                return Ok(false);
            }
        }
        cleanup_workspace_path(source_repo, workspace_path).await?;
        Ok(true)
    }

    pub async fn force_reclaim_workspace(
        &self,
        task_store: &crate::task_runner::TaskStore,
        source_repo: &Path,
        workspace_path: &Path,
    ) -> anyhow::Result<bool> {
        let _repository_lease = self
            .acquire_repository_write_lease_for_cleanup(source_repo)
            .await?;
        let _git_ops = self.git_ops.lock().await;
        let outcome = try_reclaim_workspace(
            source_repo,
            workspace_path,
            self.lease_store.as_deref(),
            None,
            WorkspaceReclaimMode::Force { task_store },
        )
        .await?;
        Ok(matches!(
            outcome,
            WorkspaceReclaimOutcome::Deleted | WorkspaceReclaimOutcome::ForcedDeleted { .. }
        ))
    }
}
