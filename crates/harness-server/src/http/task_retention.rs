use std::sync::Arc;

use chrono::Utc;

use crate::http::AppState;

const CONFIG_RETRY_SECS: u64 = 30;

/// Spawn the terminal-task retention loop.
///
/// Prunes terminal task rows and task-owned child rows (artifacts, prompts,
/// checkpoints) older than the configured cutoff. The first passes after
/// process start run in dry-run mode, reporting what would be deleted, so
/// activation on existing deployments cannot surprise operators (GH-1879).
pub(super) fn spawn_task_retention(state: &Arc<AppState>) {
    let weak_state = Arc::downgrade(state);
    tokio::spawn(async move {
        let mut dry_run_passes_remaining: Option<u32> = None;
        loop {
            let Some(state) = weak_state.upgrade() else {
                break;
            };
            let workflow_cfg = match harness_core::config::workflow::load_workflow_config(
                &state.core.project_root,
            ) {
                Ok(config) => config,
                Err(error) => {
                    tracing::warn!("task retention config load failed: {error}");
                    drop(state);
                    tokio::time::sleep(std::time::Duration::from_secs(CONFIG_RETRY_SECS)).await;
                    continue;
                }
            };
            let interval = std::time::Duration::from_secs(
                workflow_cfg.storage.task_retention_interval_secs.max(1),
            );
            if workflow_cfg.storage.task_retention_enabled {
                let dry_run_remaining = dry_run_passes_remaining
                    .get_or_insert(workflow_cfg.storage.task_retention_dry_run_passes);
                let cutoff = Utc::now()
                    - chrono::Duration::days(workflow_cfg.storage.task_retention_days as i64);
                if *dry_run_remaining > 0 {
                    *dry_run_remaining -= 1;
                    match state
                        .core
                        .tasks
                        .count_terminal_tasks_before(
                            cutoff,
                            workflow_cfg.storage.task_retention_batch_size,
                        )
                        .await
                    {
                        Ok(candidates) if candidates > 0 => tracing::info!(
                            dry_run_passes_remaining = *dry_run_remaining,
                            candidate_tasks = candidates,
                            retention_days = workflow_cfg.storage.task_retention_days,
                            "task retention dry-run: next passes will prune this many terminal tasks"
                        ),
                        Ok(_) => {}
                        Err(error) => tracing::warn!("task retention dry-run count failed: {error}"),
                    }
                } else {
                    match state
                        .core
                        .tasks
                        .prune_terminal_tasks_before(
                            cutoff,
                            workflow_cfg.storage.task_retention_batch_size,
                        )
                        .await
                    {
                        Ok(summary) if !summary.pruned_task_ids.is_empty() => tracing::info!(
                            tasks = summary.tasks_deleted,
                            artifacts = summary.artifacts_deleted,
                            prompts = summary.prompts_deleted,
                            checkpoints = summary.checkpoints_deleted,
                            "task retention pruned terminal task history"
                        ),
                        Ok(_) => {}
                        Err(error) => tracing::warn!("task retention tick failed: {error}"),
                    }
                }
            } else {
                tracing::debug!("task retention disabled by config; re-checking next interval");
            }

            drop(state);
            tokio::time::sleep(interval).await;
        }
    });
}
