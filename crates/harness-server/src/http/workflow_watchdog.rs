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
        // workflow_id -> last observed stopped state; alerts fire on the
        // transition edge only (GH1582 T006), not on every tick. Restart
        // re-observes current stopped states once; dispatcher dedup absorbs
        // the repeat within its cooldown window.
        let mut alerted_states: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();
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

                    // External alerts for stopped workflows (GH1582 T006).
                    if state.observability.alerts.is_enabled() {
                        match store
                            .list_aged_wait_instances(
                                &["blocked", "failed"],
                                Utc::now(),
                                workflow_cfg.storage.workflow_watchdog_batch_size as i64,
                            )
                            .await
                        {
                            Ok(stopped) => {
                                let batch_full = stopped.len() as i64
                                    >= workflow_cfg.storage.workflow_watchdog_batch_size as i64;
                                let mut current: std::collections::HashMap<String, String> =
                                    std::collections::HashMap::new();
                                for workflow in stopped {
                                    current.insert(workflow.id.clone(), workflow.state.clone());
                                    let seen = alerted_states.get(&workflow.id);
                                    if seen.map(String::as_str) == Some(workflow.state.as_str()) {
                                        continue;
                                    }
                                    let class = if workflow.state == "failed" {
                                        harness_core::alert::AlertClass::WorkflowFailed
                                    } else {
                                        harness_core::alert::AlertClass::WorkflowBlocked
                                    };
                                    let reason = workflow
                                        .data
                                        .get("last_stop")
                                        .and_then(|s| s.get("reason"))
                                        .and_then(serde_json::Value::as_str)
                                        .unwrap_or("no structured stop reason");
                                    state.observability.alerts.raise(
                                        crate::alerting::producers::workflow_stopped(
                                            class,
                                            &workflow.id,
                                            workflow
                                                .data
                                                .get("issue_number")
                                                .and_then(serde_json::Value::as_u64)
                                                .map(|n| n.to_string())
                                                .as_deref(),
                                            workflow
                                                .data
                                                .get("repo")
                                                .and_then(serde_json::Value::as_str),
                                            reason,
                                        ),
                                    );
                                }
                                if batch_full {
                                    // Partial view: the batch limit truncated
                                    // the stopped set, so absence from
                                    // `current` proves nothing. Merge without
                                    // evicting to avoid re-alert flapping as
                                    // workflows rotate through batches.
                                    alerted_states.extend(current);
                                } else {
                                    // Complete view: safe to drop entries for
                                    // workflows that left the stopped set so
                                    // a later re-entry alerts again. Also
                                    // bounds the map size.
                                    alerted_states = current;
                                }
                            }
                            Err(error) => {
                                tracing::warn!("workflow alert scan failed: {error}")
                            }
                        }
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
