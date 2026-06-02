//! Background orphan path-schema reaper (storage RFC Phase 3b wiring).
//!
//! `reap_orphaned_path_schemas` was added in #1216 but only exposed via the
//! `harness-pg-schema-cleanup` CLI — it was never wired into the running server,
//! so Postgres catalog growth was never actually bounded automatically (a
//! declaration-execution gap). This module closes that gap: it periodically
//! drops path-derived schemas whose owning workspace directory has been removed.
//!
//! The reap rule is conservative (only a definitively-missing owner parent
//! directory counts as orphaned; transient IO errors keep the schema), so a live
//! store is never dropped. Gated by `storage.orphan_reaper_enabled` and paced by
//! `storage.orphan_reaper_interval_secs` in `WORKFLOW.md`.

use std::sync::Arc;

use crate::http::AppState;

/// Backoff before retrying when `WORKFLOW.md` config cannot be loaded.
const CONFIG_RETRY_SECS: u64 = 30;

/// Spawn the periodic orphan-schema reaper. No-op if the workflow runtime store
/// (which provides the Postgres pool) is unavailable.
pub(super) fn spawn_orphan_schema_reaper(state: &Arc<AppState>) {
    if state.core.workflow_runtime_store.is_none() {
        tracing::debug!("orphan schema reaper disabled: workflow runtime store unavailable");
        return;
    }

    let weak_state = Arc::downgrade(state);
    tokio::spawn(async move {
        loop {
            let state = match weak_state.upgrade() {
                Some(state) => state,
                None => break,
            };

            let workflow_cfg = match harness_core::config::workflow::load_workflow_config(
                &state.core.project_root,
            ) {
                Ok(config) => config,
                Err(_) => {
                    drop(state);
                    tokio::time::sleep(std::time::Duration::from_secs(CONFIG_RETRY_SECS)).await;
                    continue;
                }
            };

            let interval = std::time::Duration::from_secs(
                workflow_cfg.storage.orphan_reaper_interval_secs.max(1),
            );

            // Disabled is a pause, not a stop: config is reloaded each iteration,
            // so re-enabling takes effect on the next tick without a restart.
            if !workflow_cfg.storage.orphan_reaper_enabled {
                tracing::debug!(
                    "orphan schema reaper disabled by config; re-checking next interval"
                );
            } else if let Some(store) = state.core.workflow_runtime_store.as_ref() {
                match harness_core::db::reap_orphaned_path_schemas(store.pool(), true).await {
                    Ok(report) if !report.orphans.is_empty() => {
                        tracing::info!(
                            scanned = report.scanned,
                            reaped = report.orphans.len(),
                            "orphan schema reaper dropped orphaned path-derived schemas"
                        );
                    }
                    Ok(_) => {}
                    Err(error) => {
                        tracing::warn!("orphan schema reaper tick failed: {error}");
                    }
                }
            }

            // Release the strong AppState ref before sleeping so a shutdown that
            // drops the last owner is not blocked for a whole interval.
            drop(state);
            tokio::time::sleep(interval).await;
        }
    });
}
