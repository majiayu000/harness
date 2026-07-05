use std::sync::Arc;

use chrono::Utc;

use crate::http::AppState;

const CONFIG_RETRY_SECS: u64 = 30;
const MAX_TASK_RETENTION_DAYS: u64 = 36_500;

pub(super) fn spawn_task_retention(state: &Arc<AppState>) {
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
                let cutoff =
                    task_retention_cutoff(Utc::now(), workflow_cfg.storage.task_retention_days);
                match state
                    .core
                    .tasks
                    .prune_terminal_tasks_before(
                        cutoff,
                        workflow_cfg.storage.task_retention_batch_size,
                    )
                    .await
                {
                    Ok(summary) if !summary.is_empty() => tracing::info!(
                        tasks = summary.tasks_deleted,
                        artifacts = summary.artifacts_deleted,
                        prompts = summary.prompts_deleted,
                        checkpoints = summary.checkpoints_deleted,
                        "task retention pruned terminal task history"
                    ),
                    Ok(_) => {}
                    Err(error) => tracing::warn!("task retention tick failed: {error}"),
                }
            } else {
                tracing::debug!("task retention disabled by config; re-checking next interval");
            }

            drop(state);
            tokio::time::sleep(interval).await;
        }
    });
}

fn task_retention_cutoff(
    now: chrono::DateTime<Utc>,
    configured_days: u64,
) -> chrono::DateTime<Utc> {
    let days = configured_days.min(MAX_TASK_RETENTION_DAYS) as i64;
    now - chrono::Duration::days(days)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_retention_cutoff_caps_large_day_values() {
        let now = chrono::DateTime::<Utc>::from(
            std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_783_209_600),
        );

        assert_eq!(
            task_retention_cutoff(now, u64::MAX),
            now - chrono::Duration::days(MAX_TASK_RETENTION_DAYS as i64)
        );
    }
}
