use std::sync::Arc;

use chrono::Utc;

use crate::http::AppState;

const CONFIG_RETRY_SECS: u64 = 30;

pub(super) fn spawn_runtime_retention(state: &Arc<AppState>) {
    if state.core.workflow_runtime_store.is_none() {
        tracing::debug!("runtime retention disabled: workflow runtime store unavailable");
        return;
    }

    let weak_state = Arc::downgrade(state);
    tokio::spawn(async move {
        loop {
            let Some(state) = weak_state.upgrade() else {
                break;
            };
            let workflow_cfg = match harness_core::config::workflow::load_workflow_config(
                &state.core.project_root,
            ) {
                Ok(config) => config,
                Err(error) => {
                    tracing::warn!("runtime retention config load failed: {error}");
                    drop(state);
                    tokio::time::sleep(std::time::Duration::from_secs(CONFIG_RETRY_SECS)).await;
                    continue;
                }
            };
            let interval = std::time::Duration::from_secs(
                workflow_cfg.storage.runtime_retention_interval_secs.max(1),
            );
            if workflow_cfg.storage.runtime_retention_enabled {
                warn_if_retention_undercuts_log_lookback(
                    workflow_cfg.storage.runtime_retention_days,
                    state.core.server.config.observe.log_retention_days.into(),
                );
                if let Some(store) = state.core.workflow_runtime_store.as_ref() {
                    let cutoff = Utc::now()
                        - chrono::Duration::days(
                            workflow_cfg.storage.runtime_retention_days as i64,
                        );
                    match store
                        .prune_terminal_runtime_history(
                            cutoff,
                            workflow_cfg.storage.runtime_retention_batch_size as i64,
                        )
                        .await
                    {
                        Ok(summary) if !summary.is_empty() => tracing::info!(
                            workflow_instances = summary.workflow_instances_deleted,
                            workflow_events = summary.workflow_events_deleted,
                            workflow_decisions = summary.workflow_decisions_deleted,
                            workflow_commands = summary.workflow_commands_deleted,
                            runtime_jobs = summary.runtime_jobs_deleted,
                            runtime_events = summary.runtime_events_deleted,
                            workflow_artifacts = summary.workflow_artifacts_deleted,
                            "runtime retention pruned terminal workflow history"
                        ),
                        Ok(_) => {}
                        Err(error) => tracing::warn!("runtime retention tick failed: {error}"),
                    }
                }
            } else {
                tracing::debug!("runtime retention disabled by config; re-checking next interval");
            }

            drop(state);
            tokio::time::sleep(interval).await;
        }
    });
}

fn warn_if_retention_undercuts_log_lookback(retention_days: u64, log_retention_days: u64) {
    if retention_days < log_retention_days {
        tracing::warn!(
            runtime_retention_days = retention_days,
            runtime_log_retention_days = log_retention_days,
            "runtime retention window is shorter than runtime log retention lookback"
        );
    }
}
