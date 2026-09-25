use std::sync::Arc;

use crate::http::AppState;

const CLEANUP_BATCH_SIZE: i64 = 32;
const CLEANUP_SWEEP_INTERVAL_SECS: u64 = 30;

pub(in crate::http) fn spawn_runtime_workspace_cleanup_sweeper(state: &Arc<AppState>) {
    if state.core.workflow_runtime_store.is_none() || state.concurrency.workspace_mgr.is_none() {
        tracing::debug!("runtime workspace cleanup sweeper disabled: required store unavailable");
        return;
    }

    let weak_state = Arc::downgrade(state);
    let handle = state
        .background_loops
        .register_loop_with_interval("runtime_workspace_cleanup", CLEANUP_SWEEP_INTERVAL_SECS);
    tokio::spawn(async move {
        let mut cursor = None;
        loop {
            let Some(state) = weak_state.upgrade() else {
                break;
            };
            match run_runtime_workspace_cleanup_sweep_tick(&state, &mut cursor).await {
                Ok(()) => handle.tick_ok(),
                Err(error) => {
                    handle.tick_failed(&error.to_string());
                    tracing::error!("runtime workspace cleanup sweep failed: {error}");
                }
            }
            drop(state);
            tokio::time::sleep(std::time::Duration::from_secs(CLEANUP_SWEEP_INTERVAL_SECS)).await;
        }
    });
}

pub(crate) async fn run_runtime_workspace_cleanup_sweep_tick(
    state: &AppState,
    cursor: &mut Option<String>,
) -> anyhow::Result<()> {
    let Some(workspace_mgr) = state.concurrency.workspace_mgr.as_ref() else {
        return Ok(());
    };
    let Some(store) = state.core.workflow_runtime_store.as_ref() else {
        return Ok(());
    };
    let workflow_ids = workspace_mgr
        .runtime_workspace_cleanup_workflow_ids_after(cursor.as_deref(), CLEANUP_BATCH_SIZE)
        .await?;
    if workflow_ids.is_empty() {
        *cursor = None;
        return Ok(());
    }

    let mut first_error = None;
    for workflow_id in &workflow_ids {
        *cursor = Some(workflow_id.clone());
        let result = match store.get_instance(workflow_id).await {
            Ok(Some(workflow))
                if workflow.is_terminal_with_registry(store.definition_registry()) =>
            {
                crate::workflow_runtime_worker::cleanup_terminal_runtime_workspace_if_uncontended(
                    state, &workflow,
                )
                .await
                .map(|_| ())
            }
            Ok(Some(_)) => Ok(()),
            Ok(None) => workspace_mgr
                .cleanup_missing_runtime_workflow_targets_if_uncontended(workflow_id)
                .await
                .map(|_| ()),
            Err(error) => Err(error),
        };
        if let Err(error) = result {
            tracing::error!(
                workflow_id,
                "runtime workspace cleanup target failed: {error}"
            );
            first_error.get_or_insert_with(|| error.to_string());
        }
    }
    if workflow_ids.len() < CLEANUP_BATCH_SIZE as usize {
        *cursor = None;
    }
    if let Some(error) = first_error {
        anyhow::bail!("one or more runtime workspace cleanup targets failed: {error}");
    }
    Ok(())
}
