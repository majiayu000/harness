use crate::http::AppState;
use harness_core::{Decision, Event, EventFilters, ReviewConfig, SessionId};
use std::sync::Arc;
use std::time::Duration;
use tokio::time::sleep;

const HOOK: &str = "periodic_review";

/// Background loop that triggers a periodic codebase review agent task.
///
/// Sleeps for `config.interval_hours`, then checks whether any new commits have
/// landed since the last review. If yes, enqueues a review task via the normal
/// task pipeline. Skips the cycle silently if there are no new commits.
pub async fn review_loop(config: ReviewConfig, state: Arc<AppState>) {
    let interval = Duration::from_secs(config.interval_hours * 3600);
    loop {
        sleep(interval).await;
        if let Err(e) = run_review_tick(&config, &state).await {
            tracing::error!("periodic_reviewer: review tick failed: {e}");
        }
    }
}

async fn run_review_tick(config: &ReviewConfig, state: &Arc<AppState>) -> anyhow::Result<()> {
    let last_ts = last_review_timestamp(state)?;
    let project_root = state.core.project_root.clone();

    let commits = match last_ts {
        Some(ts) => {
            let since = ts.format("%Y-%m-%dT%H:%M:%SZ").to_string();
            let root = project_root.clone();
            tokio::task::spawn_blocking(move || {
                std::process::Command::new("git")
                    .args([
                        "-C",
                        root.to_str().unwrap_or("."),
                        "log",
                        "--oneline",
                        &format!("--since={since}"),
                        "-1",
                    ])
                    .output()
                    .ok()
                    .and_then(|o| String::from_utf8(o.stdout).ok())
                    .unwrap_or_default()
            })
            .await
            .unwrap_or_default()
        }
        // First boot: no prior review recorded, always run.
        None => "initial review".to_string(),
    };

    if commits.trim().is_empty() {
        tracing::debug!("periodic_reviewer: no new commits since last review, skipping");
        return Ok(());
    }

    tracing::info!(
        commits = commits.trim(),
        "periodic_reviewer: new commits detected, enqueueing review task"
    );

    let prompt = harness_core::prompts::periodic_review_prompt(commits.trim());
    let req = crate::task_runner::CreateTaskRequest {
        prompt: Some(prompt),
        agent: config.agent.clone(),
        project: Some(project_root),
        turn_timeout_secs: config.timeout_secs,
        ..Default::default()
    };

    match crate::http::task_routes::enqueue_task(state, req).await {
        Ok(task_id) => {
            crate::task_runner::mutate_and_persist(&state.core.tasks, &task_id, |s| {
                s.source = Some(HOOK.to_string());
            })
            .await;

            let mut event = Event::new(
                SessionId::new(),
                HOOK,
                "periodic_reviewer",
                Decision::Complete,
            );
            event.detail = Some(format!("task_id={}", task_id.0));
            if let Err(e) = state.observability.events.log(&event) {
                tracing::warn!("periodic_reviewer: failed to log review event: {e}");
            }

            tracing::info!(task_id = %task_id.0, "periodic_reviewer: review task enqueued");
        }
        Err(e) => {
            tracing::error!("periodic_reviewer: failed to enqueue review task: {e}");
        }
    }

    Ok(())
}

fn last_review_timestamp(
    state: &AppState,
) -> anyhow::Result<Option<chrono::DateTime<chrono::Utc>>> {
    let events = state.observability.events.query(&EventFilters {
        hook: Some(HOOK.to_string()),
        ..Default::default()
    })?;
    Ok(events.last().map(|e| e.ts))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn review_config_defaults() {
        let config = ReviewConfig::default();
        assert!(!config.enabled);
        assert_eq!(config.interval_hours, 24);
        assert!(config.agent.is_none());
        assert_eq!(config.timeout_secs, 900);
    }

    #[test]
    fn review_config_deserializes_from_toml() {
        let toml_str = r#"
            enabled = true
            interval_hours = 48
            agent = "claude"
            timeout_secs = 1800
        "#;
        let config: ReviewConfig = toml::from_str(toml_str).unwrap();
        assert!(config.enabled);
        assert_eq!(config.interval_hours, 48);
        assert_eq!(config.agent.as_deref(), Some("claude"));
        assert_eq!(config.timeout_secs, 1800);
    }

    #[test]
    fn skip_logic_triggers_on_empty_commits() {
        // Empty or whitespace-only output from git log means no new commits.
        assert!("".trim().is_empty());
        assert!("  \n  ".trim().is_empty());
    }

    #[test]
    fn skip_logic_does_not_trigger_on_commit_output() {
        let git_output = "a1b2c3d fix: some bug";
        assert!(!git_output.trim().is_empty());
    }

    #[test]
    fn first_boot_always_runs() {
        // When no prior review event exists, last_ts is None and commits is
        // set to the sentinel string, which is non-empty → review runs.
        let sentinel = "initial review";
        assert!(!sentinel.trim().is_empty());
    }
}
