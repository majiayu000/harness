use super::*;

pub(super) async fn claim_cleanup_hook_once(
    lease_store: Option<&WorkspaceLeaseStore>,
    cleanup_operation: Option<&WorkspaceCleanupOperation>,
    runtime_workflow_id: Option<&str>,
    workspace_path: &Path,
    hook: WorkspaceCleanupHook,
) -> anyhow::Result<bool> {
    if let (Some(store), Some(runtime_workflow_id)) = (lease_store, runtime_workflow_id) {
        if let Some(claimed) = store
            .claim_workspace_cleanup_hook(runtime_workflow_id, workspace_path, hook)
            .await?
        {
            if let Some(cleanup_operation) = cleanup_operation {
                match hook {
                    WorkspaceCleanupHook::Workflow => {
                        cleanup_operation.claim_workflow_hook();
                    }
                    WorkspaceCleanupHook::Manager => {
                        cleanup_operation.claim_manager_hook();
                    }
                }
            }
            return Ok(claimed);
        }
    }
    Ok(
        cleanup_operation.is_none_or(|cleanup_operation| match hook {
            WorkspaceCleanupHook::Workflow => cleanup_operation.claim_workflow_hook(),
            WorkspaceCleanupHook::Manager => cleanup_operation.claim_manager_hook(),
        }),
    )
}

pub(super) async fn run_workspace_cleanup_hook(
    task_id: &TaskId,
    label: &str,
    hook: Option<&str>,
    hook_timeout_secs: u64,
    workspace_path: &Path,
) {
    let Some(hook) = hook else {
        return;
    };
    match timeout(
        Duration::from_secs(hook_timeout_secs),
        run_hook(hook, workspace_path),
    )
    .await
    {
        Ok(Ok(())) => {}
        Ok(Err(error)) => tracing::warn!(
            task_id = %task_id.0,
            "{label} hook failed during workspace cleanup: {error}"
        ),
        Err(_) => tracing::warn!(
            task_id = %task_id.0,
            "{label} hook timed out during workspace cleanup"
        ),
    }
}

impl WorkspaceManager {
    pub(crate) fn release_pool_permit_for_cleanup(&self, active: &mut ActiveWorkspace) {
        if self.lease_store.is_some() {
            active._pool_permit = None;
        }
    }

    pub(crate) async fn run_workspace_cleanup_hooks_once(
        &self,
        cleanup_operation: Option<&WorkspaceCleanupOperation>,
        task_id: &TaskId,
        runtime_workflow_id: Option<&str>,
        workflow_hook: Option<&str>,
        workflow_hook_timeout_secs: u64,
        workspace_path: &Path,
    ) -> anyhow::Result<()> {
        if workflow_hook.is_some()
            && claim_cleanup_hook_once(
                self.lease_store.as_deref(),
                cleanup_operation,
                runtime_workflow_id,
                workspace_path,
                WorkspaceCleanupHook::Workflow,
            )
            .await?
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
            && claim_cleanup_hook_once(
                self.lease_store.as_deref(),
                cleanup_operation,
                runtime_workflow_id,
                workspace_path,
                WorkspaceCleanupHook::Manager,
            )
            .await?
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
        Ok(())
    }
}
