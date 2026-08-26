use super::*;

#[allow(clippy::too_many_arguments)]
pub(super) async fn create_runtime_worktree_with_admission(
    state: &Arc<AppState>,
    job: &RuntimeJob,
    workflow: Option<&WorkflowInstance>,
    workspace_mgr: &crate::workspace::WorkspaceManager,
    task_id: &TaskId,
    source_project_root: &Path,
    workflow_document: &WorkflowDocument,
    execution_cancelled: tokio::sync::watch::Receiver<bool>,
    external_id: Option<&str>,
    repo: Option<&str>,
    reuse_existing_workspace: bool,
) -> anyhow::Result<crate::workspace::WorkspaceLease> {
    loop {
        let (repository_write_lease, workspace_capacity) =
            acquire_runtime_repository_lease(state, workspace_mgr, source_project_root).await?;
        if repository_write_lease.is_some() {
            revalidate_runtime_workspace_admission(state, job, workflow).await?;
        }
        let repository_lease_lost = repository_write_lease
            .as_ref()
            .map(RepositoryWriteLease::loss_receiver);
        let options = crate::workspace::WorkspaceCreateOptions {
            require_remote_head: workflow_document.config.base.require_remote_head,
            reuse_existing_workspace,
            after_create_hook: workflow_document.config.hooks.after_create.clone(),
            before_remove_hook: workflow_document.config.hooks.before_remove.clone(),
            hook_timeout_secs: Some(workflow_document.config.hooks.timeout_secs),
            branch_prefix: workflow_document.config.workspace.branch_prefix.clone(),
            runtime_workflow_id: workflow.map(|workflow| workflow.id.clone()),
            persist_runtime_cleanup_target: workflow_document.config.workspace.cleanup
                == "on_terminal",
            workspace_capacity_override: Some(workspace_capacity),
            repository_write_lease: repository_write_lease.map_or(
                crate::workspace::RepositoryWriteLeaseInput::NotRequired,
                crate::workspace::RepositoryWriteLeaseInput::Held,
            ),
        };
        let result = run_preparation_phase(
            repository_lease_lost,
            execution_cancelled.clone(),
            Box::pin(async {
                workspace_mgr
                    .create_workspace_with_options(
                        task_id,
                        source_project_root,
                        &workflow_document.config.base.remote,
                        &workflow_document.config.base.branch,
                        1,
                        external_id,
                        repo,
                        options,
                    )
                    .await
                    .map_err(anyhow::Error::new)
            }),
            "runtime workspace preparation",
        )
        .await;
        match result {
            Err(error)
                if matches!(
                    error.downcast_ref::<crate::workspace::WorkspaceLifecycleError>(),
                    Some(crate::workspace::WorkspaceLifecycleError::PersistedSlotContended)
                ) =>
            {
                tokio::time::sleep(TokioDuration::from_millis(250)).await;
            }
            result => return result,
        }
    }
}
