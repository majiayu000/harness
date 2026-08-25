use super::*;

pub(crate) struct RuntimeWorkspaceCleanupTarget {
    pub(crate) task_id: TaskId,
    pub(crate) workspace_path: PathBuf,
    persisted_target: Option<crate::workspace_lease_store::WorkspaceCleanupTargetRecord>,
}

pub(crate) struct RuntimeWorkspaceCleanupClaim {
    persisted: Option<crate::workspace_lease_store::PersistedWorkspaceCleanupClaim>,
}

impl RuntimeWorkspaceCleanupClaim {
    pub(crate) async fn complete(
        mut self,
        manager: &WorkspaceManager,
        target: &RuntimeWorkspaceCleanupTarget,
    ) -> anyhow::Result<()> {
        if let Some(persisted) = self.persisted.take() {
            persisted.complete().await?;
        } else {
            manager.release_workspace(&target.task_id).await;
        }
        Ok(())
    }

    fn loss_receiver(&self) -> Option<tokio::sync::watch::Receiver<bool>> {
        self.persisted
            .as_ref()
            .map(crate::workspace_lease_store::PersistedWorkspaceCleanupClaim::loss_receiver)
    }
}

impl RuntimeWorkspaceCleanupTarget {
    pub(crate) fn acquisition_id(&self) -> Option<&str> {
        self.persisted_target
            .as_ref()
            .and_then(|target| target.acquisition_id.as_deref())
    }

    pub(crate) fn is_persisted(&self) -> bool {
        self.persisted_target.is_some()
    }
}

pub(crate) enum RepositoryWriteLeaseAttempt {
    NotRequired,
    Acquired(RepositoryWriteLease),
    Contended,
}

pub(crate) enum ResolvedWorkspaceAdmission {
    Reused(WorkspaceLease),
    Acquired {
        repository_write_lease: Option<RepositoryWriteLease>,
        workspace_project_key: String,
        capacity: usize,
        pool_permit: OwnedSemaphorePermit,
        slot_guard: tokio::sync::OwnedMutexGuard<()>,
    },
}

fn repository_admission_error(error: anyhow::Error) -> WorkspaceLifecycleError {
    WorkspaceLifecycleError::CreateFailed {
        message: format!("failed to refresh live repository admission: {error}"),
    }
}

impl WorkspaceManager {
    pub(super) async fn acquire_resolved_workspace_admission(
        &self,
        task_id: &TaskId,
        source_repo: &Path,
        repo: Option<&str>,
        run_generation: u32,
        mut repository_write_lease: Option<RepositoryWriteLease>,
        capacity: usize,
    ) -> Result<ResolvedWorkspaceAdmission, WorkspaceLifecycleError> {
        let project_key = crate::workspace_pool::project_limit_key(source_repo);
        if self
            .repository_admission_is_stale(source_repo, capacity, repository_write_lease.as_ref())
            .await
            .map_err(repository_admission_error)?
        {
            drop(repository_write_lease.take());
            return Err(WorkspaceLifecycleError::CreateFailed {
                message: "live workspace capacity changed before admission; retry acquisition"
                    .to_string(),
            });
        }
        if let Some(lease) = self
            .try_reuse_active_workspace(
                task_id,
                run_generation,
                capacity,
                &mut repository_write_lease,
            )
            .await?
        {
            return Ok(ResolvedWorkspaceAdmission::Reused(lease));
        }
        let permit = self
            .pool
            .acquire_with_capacity(source_repo, repo, capacity)
            .await
            .map_err(|error| WorkspaceLifecycleError::CreateFailed {
                message: format!(
                    "workspace pool acquisition failed for task {}: {error}",
                    task_id.0
                ),
            })?;
        debug_assert_eq!(project_key, permit.project_key);
        let slot_guard = self.pool.selection_lock(&project_key).lock_owned().await;
        if self
            .repository_admission_is_stale(
                source_repo,
                permit.capacity,
                repository_write_lease.as_ref(),
            )
            .await
            .map_err(repository_admission_error)?
        {
            drop(slot_guard);
            drop(permit);
            drop(repository_write_lease.take());
            return Err(WorkspaceLifecycleError::CreateFailed {
                message: "live workspace capacity changed during admission wait; retry acquisition"
                    .to_string(),
            });
        }
        if let Some(lease) = self
            .try_reuse_active_workspace(
                task_id,
                run_generation,
                permit.capacity,
                &mut repository_write_lease,
            )
            .await?
        {
            return Ok(ResolvedWorkspaceAdmission::Reused(lease));
        }
        Ok(ResolvedWorkspaceAdmission::Acquired {
            repository_write_lease,
            workspace_project_key: permit.workspace_project_key,
            capacity: permit.capacity,
            pool_permit: permit.permit,
            slot_guard,
        })
    }

    pub(super) async fn create_workspace_with_resolved_repository_lease(
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
        self.prepare_active_repository_lease_resolution(task_id, run_generation)?;
        let project_key = crate::workspace_pool::project_limit_key(source_repo);
        let (lease, capacity) = self
            .acquire_repository_lease_from_current_config(source_repo)
            .await
            .map_err(|error| WorkspaceLifecycleError::CreateFailed {
                message: format!(
                    "failed to acquire PostgreSQL repository lease for {project_key}: {error}"
                ),
            })?;
        let repository_lease_lost = lease.as_ref().map(RepositoryWriteLease::loss_receiver);
        let repository_write_lease = lease.map_or(
            RepositoryWriteLeaseInput::NotRequired,
            RepositoryWriteLeaseInput::Held,
        );
        let monitored_options = WorkspaceCreateOptions {
            require_remote_head: options.require_remote_head,
            reuse_existing_workspace: options.reuse_existing_workspace,
            after_create_hook: options.after_create_hook,
            before_remove_hook: options.before_remove_hook,
            hook_timeout_secs: options.hook_timeout_secs,
            branch_prefix: options.branch_prefix,
            runtime_workflow_id: options.runtime_workflow_id,
            persist_runtime_cleanup_target: options.persist_runtime_cleanup_target,
            workspace_capacity_override: Some(capacity),
            repository_write_lease,
        };
        run_until_repository_lease_loss(
            repository_lease_lost,
            Box::pin(self.create_workspace_with_options(
                task_id,
                source_repo,
                remote,
                base_branch,
                run_generation,
                external_id,
                repo,
                monitored_options,
            )),
        )
        .await
        .ok_or_else(|| WorkspaceLifecycleError::CreateFailed {
            message: "repository lease was lost during workspace creation".to_string(),
        })?
    }

    pub(crate) async fn runtime_workspace_cleanup_workflow_ids_after(
        &self,
        after: Option<&str>,
        limit: i64,
    ) -> anyhow::Result<Vec<String>> {
        let Some(store) = self.lease_store.as_ref() else {
            return Ok(Vec::new());
        };
        store
            .runtime_workspace_cleanup_workflow_ids_after(after, limit)
            .await
    }

    pub(crate) fn discard_unhealthy_repository_lease(&self, task_id: &TaskId) {
        if let Some(mut active) = self.active.get_mut(task_id) {
            let unhealthy = active
                ._repository_write_lease
                .as_ref()
                .is_some_and(|lease| !lease.is_healthy());
            if unhealthy {
                active._repository_write_lease = None;
            }
        }
    }

    pub(crate) fn attach_repository_lease(
        active: &mut ActiveWorkspace,
        candidate: &mut Option<RepositoryWriteLease>,
    ) {
        let candidate_mode = candidate.as_ref().map(RepositoryWriteLease::mode);
        let existing_matches = active
            ._repository_write_lease
            .as_ref()
            .is_some_and(|lease| lease.is_healthy() && Some(lease.mode()) == candidate_mode);
        if !existing_matches {
            active._repository_write_lease = candidate.take();
        }
    }

    pub(crate) fn repository_lease_lost_for_task(
        &self,
        task_id: &TaskId,
    ) -> Option<tokio::sync::watch::Receiver<RepositoryLeaseState>> {
        self.active.get(task_id).and_then(|active| {
            active
                ._repository_write_lease
                .as_ref()
                .map(RepositoryWriteLease::loss_receiver)
        })
    }

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

    pub(crate) async fn repository_admission_is_stale(
        &self,
        source_repo: &Path,
        admitted_capacity: usize,
        lease: Option<&RepositoryWriteLease>,
    ) -> anyhow::Result<bool> {
        if self.capacity_source.is_none() {
            return Ok(false);
        }
        let current_capacity = self.resolve_workspace_capacity(source_repo).await?;
        let current_mode = self
            .pool
            .repository_lease_mode_for_capacity(current_capacity);
        Ok(current_capacity != admitted_capacity
            || match (current_mode, lease) {
                (None, None) => false,
                (Some(mode), Some(lease)) => !lease.is_healthy() || lease.mode() != mode,
                (None, Some(_)) | (Some(_), None) => true,
            })
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
        match mode {
            RepositoryLeaseMode::Shared => {
                store
                    .acquire_queued_repository_shared_lease(&project_key)
                    .await
            }
            RepositoryLeaseMode::Exclusive => {
                store
                    .acquire_queued_repository_write_lease(&project_key)
                    .await
            }
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
                && (current.task_id != record.task_id
                    || current.owner_session != record.owner_session
                    || current.run_generation != record.run_generation
                    || current.acquisition_id != record.acquisition_id
                    || current.process_id != record.process_id
                    || current.process_started_at != record.process_started_at
                    || current.runtime_workflow_id.as_deref()
                        != Some(record.runtime_workflow_id.as_str()))
        }))
    }

    pub(crate) async fn claim_runtime_workspace_cleanup_target(
        &self,
        target: &RuntimeWorkspaceCleanupTarget,
    ) -> anyhow::Result<Option<RuntimeWorkspaceCleanupClaim>> {
        let (Some(store), Some(record)) =
            (self.lease_store.as_ref(), target.persisted_target.as_ref())
        else {
            return Ok(Some(RuntimeWorkspaceCleanupClaim { persisted: None }));
        };
        let claim_id = SessionId::new().to_string();
        Ok(
            crate::workspace_lease_store::PersistedWorkspaceCleanupClaim::claim(
                store.clone(),
                record.clone(),
                claim_id,
                self.owner_session.clone(),
                std::process::id(),
                WorkspaceLeaseStore::current_process_started_at()?,
            )
            .await?
            .map(|persisted| RuntimeWorkspaceCleanupClaim {
                persisted: Some(persisted),
            }),
        )
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

    pub(crate) async fn cleanup_missing_runtime_workflow_targets_if_uncontended(
        &self,
        runtime_workflow_id: &str,
    ) -> anyhow::Result<usize> {
        let targets = self
            .workspace_targets_for_runtime_workflow(runtime_workflow_id)
            .await?;
        let mut completed = 0;
        let mut first_error = None;
        for target in targets {
            match self.cleanup_missing_runtime_workflow_target(&target).await {
                Ok(true) => completed += 1,
                Ok(false) => {}
                Err(error) => {
                    tracing::error!(
                        workflow_id = runtime_workflow_id,
                        workspace_path = %target.workspace_path.display(),
                        "runtime workspace cleanup target failed: {error}"
                    );
                    first_error.get_or_insert_with(|| error.to_string());
                }
            }
        }
        if let Some(error) = first_error {
            anyhow::bail!(
                "one or more missing-workflow cleanup targets failed after processing the batch: {error}"
            );
        }
        Ok(completed)
    }

    async fn cleanup_missing_runtime_workflow_target(
        &self,
        target: &RuntimeWorkspaceCleanupTarget,
    ) -> anyhow::Result<bool> {
        let Some(record) = target.persisted_target.as_ref() else {
            return Ok(false);
        };
        if self
            .runtime_workspace_cleanup_target_is_superseded(target)
            .await?
        {
            self.release_runtime_workspace_cleanup_target(target)
                .await?;
            return Ok(true);
        }
        ensure_workspace_cleanup_path_within_root(&self.config.root, &target.workspace_path)?;
        let repository_write_lease = match self
            .try_acquire_repository_write_lease_for_reconciliation(&record.source_repo)
            .await?
        {
            RepositoryWriteLeaseAttempt::NotRequired => None,
            RepositoryWriteLeaseAttempt::Acquired(lease) => Some(lease),
            RepositoryWriteLeaseAttempt::Contended => return Ok(false),
        };
        let mut receiver = repository_write_lease
            .as_ref()
            .map(RepositoryWriteLease::loss_receiver);
        if self
            .runtime_workspace_cleanup_target_is_superseded(target)
            .await?
        {
            self.release_runtime_workspace_cleanup_target(target)
                .await?;
            return Ok(true);
        }
        if receiver
            .as_ref()
            .is_some_and(|receiver| *receiver.borrow() != RepositoryLeaseState::Healthy)
        {
            return Ok(false);
        }
        let Some(cleanup_claim) = self.claim_runtime_workspace_cleanup_target(target).await? else {
            return Ok(false);
        };
        let mut cleanup_claim_lost = cleanup_claim.loss_receiver();
        let cleanup = async {
            if let Some(acquisition_id) = target.acquisition_id() {
                self.cleanup_workspace_acquisition_for_retry(
                    &target.task_id,
                    &record.source_repo,
                    &target.workspace_path,
                    acquisition_id,
                )
                .await
            } else if self
                .active_workspace_acquisition_id(&target.task_id, &target.workspace_path)
                .is_some()
            {
                Ok(WorkspaceRetryCleanupOutcome::DeferredActive)
            } else {
                self.cleanup_workspace_for_retry(
                    &target.task_id,
                    &record.source_repo,
                    Some(&target.workspace_path),
                )
                .await
            }
        };
        tokio::pin!(cleanup);
        let outcome = {
            let repository_lease_loss = async {
                match receiver.as_mut() {
                    Some(receiver) => wait_for_repository_lease_loss(receiver).await,
                    None => std::future::pending::<()>().await,
                }
            };
            let cleanup_claim_loss = async {
                match cleanup_claim_lost.as_mut() {
                    Some(receiver) => wait_for_cleanup_claim_loss(receiver).await,
                    None => std::future::pending::<()>().await,
                }
            };
            tokio::pin!(repository_lease_loss);
            tokio::pin!(cleanup_claim_loss);
            tokio::select! {
                biased;
                () = &mut repository_lease_loss => return Ok(false),
                () = &mut cleanup_claim_loss => return Ok(false),
                result = &mut cleanup => result?,
            }
        };
        if receiver.as_ref().is_some_and(|receiver| {
            matches!(
                *receiver.borrow(),
                RepositoryLeaseState::Revoking | RepositoryLeaseState::Lost
            )
        }) || outcome == WorkspaceRetryCleanupOutcome::DeferredActive
        {
            return Ok(false);
        }
        cleanup_claim.complete(self, target).await?;
        Ok(true)
    }

    pub(crate) async fn cleanup_reconciliation_workspace_path(
        &self,
        source_repo: &Path,
        workspace_path: &Path,
    ) -> anyhow::Result<bool> {
        let repository_lease = self
            .acquire_repository_write_lease_for_cleanup(source_repo)
            .await?;
        let repository_lease_lost = repository_lease
            .as_ref()
            .map(RepositoryWriteLease::loss_receiver);
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
        let cleanup = run_until_repository_lease_loss(
            repository_lease_lost,
            cleanup_workspace_path(source_repo, workspace_path),
        )
        .await;
        match cleanup {
            Some(result) => result.map(|()| true),
            None => Ok(false),
        }
    }

    pub async fn force_reclaim_workspace(
        &self,
        task_store: &crate::task_runner::TaskStore,
        source_repo: &Path,
        workspace_path: &Path,
    ) -> anyhow::Result<bool> {
        let repository_lease = self
            .acquire_repository_write_lease_for_cleanup(source_repo)
            .await?;
        let repository_lease_lost = repository_lease
            .as_ref()
            .map(RepositoryWriteLease::loss_receiver);
        let _git_ops = self.git_ops.lock().await;
        let outcome = run_until_repository_lease_loss(
            repository_lease_lost,
            try_reclaim_workspace(
                source_repo,
                workspace_path,
                self.lease_store.as_deref(),
                None,
                WorkspaceReclaimMode::Force { task_store },
            ),
        )
        .await
        .ok_or_else(|| anyhow::anyhow!("repository lease was lost during forced reclamation"))??;
        Ok(matches!(
            outcome,
            WorkspaceReclaimOutcome::Deleted | WorkspaceReclaimOutcome::ForcedDeleted { .. }
        ))
    }
}

pub(crate) async fn run_until_repository_lease_loss<T, F>(
    receiver: Option<tokio::sync::watch::Receiver<RepositoryLeaseState>>,
    future: F,
) -> Option<T>
where
    F: std::future::Future<Output = T>,
{
    let Some(mut receiver) = receiver else {
        return Some(future.await);
    };
    if *receiver.borrow() != RepositoryLeaseState::Healthy {
        return None;
    }
    tokio::pin!(future);
    tokio::select! {
        biased;
        () = wait_for_repository_lease_loss(&mut receiver) => None,
        result = &mut future => {
            matches!(
                *receiver.borrow(),
                RepositoryLeaseState::Healthy | RepositoryLeaseState::Released
            ).then_some(result)
        }
    }
}

async fn wait_for_repository_lease_loss(
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

async fn wait_for_cleanup_claim_loss(receiver: &mut tokio::sync::watch::Receiver<bool>) {
    loop {
        if *receiver.borrow() {
            return;
        }
        if receiver.changed().await.is_err() {
            return;
        }
    }
}

#[cfg(test)]
mod repository_lease_execution_tests {
    use super::*;

    #[tokio::test]
    async fn repository_revocation_interrupts_in_flight_mutation() {
        let (state_tx, state_rx) = tokio::sync::watch::channel(RepositoryLeaseState::Healthy);
        let mutation = tokio::spawn(run_until_repository_lease_loss(
            Some(state_rx),
            std::future::pending::<()>(),
        ));
        tokio::task::yield_now().await;

        state_tx
            .send(RepositoryLeaseState::Revoking)
            .expect("send revocation");

        assert!(mutation.await.expect("mutation task").is_none());
    }
}
