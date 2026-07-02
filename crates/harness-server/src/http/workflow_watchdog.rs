use std::sync::Arc;

use chrono::Utc;

use crate::http::AppState;

const CONFIG_RETRY_SECS: u64 = 30;
const WAIT_STATES: &[&str] = &["blocked", "awaiting_feedback"];

pub(super) fn spawn_workflow_watchdog(state: &Arc<AppState>) {
    if state.core.workflow_runtime_store.is_none() {
        tracing::debug!("workflow watchdog disabled: workflow runtime store unavailable");
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
                    tracing::warn!("workflow watchdog config load failed: {error}");
                    drop(state);
                    tokio::time::sleep(std::time::Duration::from_secs(CONFIG_RETRY_SECS)).await;
                    continue;
                }
            };
            let interval = std::time::Duration::from_secs(
                workflow_cfg.storage.workflow_watchdog_interval_secs.max(1),
            );
            if workflow_cfg.storage.workflow_watchdog_enabled {
                if let Some(store) = state.core.workflow_runtime_store.as_ref() {
                    let cutoff = Utc::now()
                        - chrono::Duration::minutes(
                            workflow_cfg.storage.workflow_watchdog_age_minutes as i64,
                        );
                    match store
                        .list_aged_wait_instances(
                            WAIT_STATES,
                            cutoff,
                            workflow_cfg.storage.workflow_watchdog_batch_size as i64,
                        )
                        .await
                    {
                        Ok(workflows) => {
                            let now = Utc::now();
                            for workflow in workflows {
                                tracing::error!(
                                    workflow_id = %workflow.id,
                                    definition_id = %workflow.definition_id,
                                    state = %workflow.state,
                                    age_secs = now
                                        .signed_duration_since(workflow.updated_at)
                                        .num_seconds()
                                        .max(0),
                                    repo = workflow
                                        .data
                                        .get("repo")
                                        .and_then(serde_json::Value::as_str),
                                    issue = workflow
                                        .data
                                        .get("issue_number")
                                        .and_then(serde_json::Value::as_u64),
                                    "workflow watchdog found aged wait-state workflow"
                                );
                            }
                        }
                        Err(error) => tracing::warn!("workflow watchdog tick failed: {error}"),
                    }
                }
            } else {
                tracing::debug!("workflow watchdog disabled by config; re-checking next interval");
            }

            drop(state);
            tokio::time::sleep(interval).await;
        }
    });
}
