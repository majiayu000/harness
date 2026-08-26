use super::*;

impl WorkspaceManager {
    pub(crate) async fn create_workspace_with_resolved_repository_lease(
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
        let WorkspaceCreateOptions {
            require_remote_head,
            reuse_existing_workspace,
            after_create_hook,
            before_remove_hook,
            hook_timeout_secs,
            branch_prefix,
            runtime_workflow_id,
            persist_runtime_cleanup_target,
            ..
        } = options;
        loop {
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
                require_remote_head,
                reuse_existing_workspace,
                after_create_hook: after_create_hook.clone(),
                before_remove_hook: before_remove_hook.clone(),
                hook_timeout_secs,
                branch_prefix: branch_prefix.clone(),
                runtime_workflow_id: runtime_workflow_id.clone(),
                persist_runtime_cleanup_target,
                workspace_capacity_override: Some(capacity),
                repository_write_lease,
            };
            let result = run_until_repository_lease_loss(
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
            })?;
            match result {
                Err(WorkspaceLifecycleError::PersistedSlotContended) => {
                    tokio::time::sleep(Duration::from_millis(250)).await;
                }
                result => return result,
            }
        }
    }
}
