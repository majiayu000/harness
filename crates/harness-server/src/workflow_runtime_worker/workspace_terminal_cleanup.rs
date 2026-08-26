use super::*;
use std::future::Future;

pub(crate) async fn repository_lease_loss_error(
    receiver: &mut tokio::sync::watch::Receiver<RepositoryLeaseState>,
) -> anyhow::Error {
    loop {
        let state = *receiver.borrow();
        match state {
            RepositoryLeaseState::Revoking | RepositoryLeaseState::Lost => {
                return anyhow::anyhow!("PostgreSQL repository advisory-lock session was lost");
            }
            RepositoryLeaseState::Released => std::future::pending::<()>().await,
            RepositoryLeaseState::Healthy => {}
        }
        if receiver.changed().await.is_err() {
            return anyhow::anyhow!("PostgreSQL repository advisory-lock monitor stopped");
        }
    }
}

pub(crate) async fn run_while_repository_lease_healthy<T, F>(
    receiver: Option<tokio::sync::watch::Receiver<RepositoryLeaseState>>,
    future: F,
    phase: &str,
) -> anyhow::Result<T>
where
    F: Future<Output = anyhow::Result<T>>,
{
    let Some(mut receiver) = receiver else {
        return future.await;
    };
    if *receiver.borrow() != RepositoryLeaseState::Healthy {
        anyhow::bail!("repository lease was not healthy before {phase}");
    }
    tokio::pin!(future);
    tokio::select! {
        biased;
        error = repository_lease_loss_error(&mut receiver) => {
            Err(anyhow::anyhow!("repository lease was lost during {phase}: {error}"))
        }
        result = &mut future => {
            let state = *receiver.borrow();
            match state {
                RepositoryLeaseState::Healthy | RepositoryLeaseState::Released => result,
                RepositoryLeaseState::Revoking | RepositoryLeaseState::Lost => {
                    Err(anyhow::anyhow!("repository lease was lost while completing {phase}"))
                }
            }
        }
    }
}

pub(crate) async fn run_preparation_phase<T, F>(
    repository_lease: Option<tokio::sync::watch::Receiver<RepositoryLeaseState>>,
    mut execution_cancelled: tokio::sync::watch::Receiver<bool>,
    future: F,
    phase: &str,
) -> anyhow::Result<T>
where
    F: Future<Output = anyhow::Result<T>>,
{
    if *execution_cancelled.borrow() {
        anyhow::bail!("runtime execution was cancelled before {phase}");
    }
    tokio::select! {
        biased;
        _ = wait_for_execution_cancellation(&mut execution_cancelled) => {
            anyhow::bail!("runtime execution was cancelled during {phase}")
        }
        result = run_while_repository_lease_healthy(repository_lease, future, phase) => result,
    }
}

async fn wait_for_execution_cancellation(receiver: &mut tokio::sync::watch::Receiver<bool>) {
    loop {
        if *receiver.borrow() || receiver.changed().await.is_err() {
            return;
        }
    }
}

pub(crate) async fn cleanup_terminal_runtime_workspace(
    state: &AppState,
    workflow: &WorkflowInstance,
) -> anyhow::Result<()> {
    cleanup_terminal_runtime_workspace_with_admission(state, workflow, true)
        .await
        .map(|_| ())
}

pub(crate) async fn cleanup_terminal_runtime_workspace_if_uncontended(
    state: &AppState,
    workflow: &WorkflowInstance,
) -> anyhow::Result<bool> {
    cleanup_terminal_runtime_workspace_with_admission(state, workflow, false).await
}

async fn cleanup_terminal_runtime_workspace_with_admission(
    state: &AppState,
    workflow: &WorkflowInstance,
    wait_for_repository_lease: bool,
) -> anyhow::Result<bool> {
    let Some(store) = state.core.workflow_runtime_store.as_ref() else {
        return Ok(true);
    };
    if !workflow.is_terminal_with_registry(store.definition_registry()) {
        return Ok(true);
    }
    let Some(workspace_mgr) = state.concurrency.workspace_mgr.as_ref() else {
        return Ok(true);
    };
    let Some(project_id) = workflow
        .data
        .get("project_id")
        .and_then(serde_json::Value::as_str)
    else {
        return Ok(true);
    };
    let source_project_root = PathBuf::from(project_id);
    let workflow_document =
        harness_core::config::workflow::load_workflow_document(&source_project_root)?;
    if workflow_document.config.workspace.strategy != "worktree" {
        return Ok(true);
    }

    let task_id = stable_runtime_workspace_task_id_for_workflow(workflow);
    let repo = workflow
        .data
        .get("repo")
        .and_then(serde_json::Value::as_str)
        .or(workflow_document.config.source.repo.as_deref());
    let repository_write_lease = if wait_for_repository_lease {
        workspace_mgr
            .acquire_repository_write_lease_for_cleanup(&source_project_root)
            .await?
    } else {
        match workspace_mgr
            .try_acquire_repository_write_lease_for_reconciliation(&source_project_root)
            .await?
        {
            crate::workspace::RepositoryWriteLeaseAttempt::NotRequired => None,
            crate::workspace::RepositoryWriteLeaseAttempt::Acquired(lease) => Some(lease),
            crate::workspace::RepositoryWriteLeaseAttempt::Contended => return Ok(false),
        }
    };
    run_while_repository_lease_healthy(
        repository_write_lease
            .as_ref()
            .map(RepositoryWriteLease::loss_receiver),
        async {
            let current_workflow = store.get_instance(&workflow.id).await?.ok_or_else(|| {
                anyhow::anyhow!(
                    "workflow {} disappeared before workspace cleanup",
                    workflow.id
                )
            })?;
            if store
                .terminal_state_for_instance(&current_workflow)
                .await?
                .is_none()
            {
                tracing::info!(
                    workflow_id = %workflow.id,
                    "skipping terminal workspace cleanup because the workflow reopened while waiting for the repository lease"
                );
                return Ok(());
            }
            let mut cleanup_targets = workspace_mgr
                .workspace_targets_for_runtime_workflow(&workflow.id)
                .await?;
            if cleanup_targets.is_empty() {
                let workspace_path = workspace_mgr
                    .workspace_path_for_cleanup(
                        &task_id,
                        &source_project_root,
                        Some(workflow.subject.subject_key.as_str()),
                        repo,
                    )
                    .await;
                crate::workspace::workspace_helpers::ensure_workspace_cleanup_path_within_root(
                    &workspace_mgr.config.root,
                    &workspace_path,
                )?;
                if workspace_path.exists() {
                    let acquisition_id = workspace_mgr
                        .active_workspace_acquisition_id(&task_id, &workspace_path);
                    if let Some(acquisition_id) = acquisition_id.as_deref() {
                        workspace_mgr
                            .cleanup_workspace_acquisition_for_retry(
                                &task_id,
                                &source_project_root,
                                &workspace_path,
                                acquisition_id,
                            )
                            .await?;
                    } else {
                        workspace_mgr
                            .cleanup_workspace_for_retry(
                            &task_id,
                            &source_project_root,
                            Some(&workspace_path),
                        )
                        .await?;
                    }
                }
                return Ok(());
            }
            for target in cleanup_targets.drain(..) {
                if workspace_mgr
                    .runtime_workspace_cleanup_target_is_superseded(&target)
                    .await?
                {
                    workspace_mgr
                        .release_runtime_workspace_cleanup_target(&target)
                        .await?;
                    continue;
                }
                crate::workspace::workspace_helpers::ensure_workspace_cleanup_path_within_root(
                    &workspace_mgr.config.root,
                    &target.workspace_path,
                )?;
                let active_acquisition_id = if target.is_persisted() {
                    None
                } else {
                    workspace_mgr
                        .active_workspace_acquisition_id(&target.task_id, &target.workspace_path)
                };
                let acquisition_id = target
                    .acquisition_id()
                    .map(str::to_owned)
                    .or(active_acquisition_id);
                let cleanup_operation = acquisition_id
                    .as_deref()
                    .map(|acquisition_id| workspace_mgr.workspace_cleanup_operation(acquisition_id));
                let serialized_locally = workspace_mgr.workspace_cleanup_uses_local_serialization();
                let cleanup_guard = match (serialized_locally, cleanup_operation.as_ref()) {
                    (true, Some(cleanup_operation)) => {
                        Some(cleanup_operation.lock.lock().await)
                    }
                    _ => None,
                };
                if serialized_locally
                    && cleanup_operation.is_some()
                    && workspace_mgr
                        .active_workspace_acquisition_id(&target.task_id, &target.workspace_path)
                        .as_deref()
                        != acquisition_id.as_deref()
                {
                    drop(cleanup_guard);
                    if let (Some(acquisition_id), Some(cleanup_operation)) =
                        (acquisition_id.as_deref(), cleanup_operation.as_ref())
                    {
                        workspace_mgr.forget_local_workspace_cleanup_operation(
                            acquisition_id,
                            cleanup_operation,
                        );
                    }
                    continue;
                }
                let Some(cleanup_claim) = workspace_mgr
                    .claim_runtime_workspace_cleanup_target(&target)
                    .await?
                else {
                    continue;
                };
                workspace_mgr
                    .run_workspace_cleanup_hooks_once(
                        cleanup_operation.as_deref(),
                        &target.task_id,
                        Some(&workflow.id),
                        workflow_document.config.hooks.before_remove.as_deref(),
                        workflow_document.config.hooks.timeout_secs,
                        &target.workspace_path,
                    )
                    .await?;
                let outcome = if let Some(acquisition_id) = acquisition_id.as_deref() {
                    if !target.is_persisted() && !target.workspace_path.exists() {
                        workspace_mgr
                            .release_workspace_acquisition(&target.task_id, acquisition_id)
                            .await?;
                        crate::workspace::WorkspaceRetryCleanupOutcome::Removed
                    } else {
                        workspace_mgr
                            .cleanup_workspace_acquisition_for_retry(
                                &target.task_id,
                                &source_project_root,
                                &target.workspace_path,
                                acquisition_id,
                            )
                            .await?
                    }
                } else if workspace_mgr
                    .active_workspace_acquisition_id(&target.task_id, &target.workspace_path)
                    .is_some()
                {
                    crate::workspace::WorkspaceRetryCleanupOutcome::DeferredActive
                } else {
                    workspace_mgr
                        .cleanup_workspace_for_retry(
                            &target.task_id,
                            &source_project_root,
                            Some(&target.workspace_path),
                        )
                        .await?
                };
                if outcome == crate::workspace::WorkspaceRetryCleanupOutcome::DeferredActive {
                    continue;
                }
                cleanup_claim.complete(workspace_mgr, &target).await?;
                drop(cleanup_guard);
                if let (Some(acquisition_id), Some(cleanup_operation)) =
                    (acquisition_id.as_deref(), cleanup_operation.as_ref())
                {
                    workspace_mgr.forget_local_workspace_cleanup_operation(
                        acquisition_id,
                        cleanup_operation,
                    );
                }
            }
            Ok(())
        },
        "terminal runtime workspace cleanup",
    )
    .await?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn preparation_is_interrupted_by_execution_cancellation() {
        let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
        let preparation = tokio::spawn(run_preparation_phase(
            None,
            cancel_rx,
            std::future::pending::<anyhow::Result<()>>(),
            "test preparation",
        ));
        tokio::task::yield_now().await;

        cancel_tx.send(true).expect("send cancellation");
        let error = preparation
            .await
            .expect("preparation task")
            .expect_err("cancellation must interrupt preparation");

        assert!(error.to_string().contains("cancelled during"));
    }

    #[tokio::test]
    async fn successful_phase_may_release_its_own_repository_lease() {
        let (lease_tx, lease_rx) = tokio::sync::watch::channel(RepositoryLeaseState::Healthy);

        let result = run_while_repository_lease_healthy(
            Some(lease_rx),
            async move {
                lease_tx
                    .send(RepositoryLeaseState::Released)
                    .expect("release lease");
                Ok::<_, anyhow::Error>("finished")
            },
            "test finalization",
        )
        .await;

        assert_eq!(result.expect("owned release is successful"), "finished");
    }
}
