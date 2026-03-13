use crate::http::task_routes;
use crate::http::AppState;
use crate::task_runner::CreateTaskRequest;
use harness_core::{Decision, Event, EventFilters, ReviewConfig, SessionId};
use std::sync::Arc;
use std::time::Duration;
use tokio::time::sleep;

/// Spawn the periodic review loop as a background task.
///
/// If review is disabled in config the loop returns immediately without
/// spawning anything; no resources are consumed.
pub fn start(state: Arc<AppState>, config: ReviewConfig) {
    if !config.enabled {
        tracing::debug!("scheduler: periodic review disabled, review_loop not started");
        return;
    }

    tokio::spawn(async move {
        review_loop(state, config).await;
    });
}

async fn review_loop(state: Arc<AppState>, config: ReviewConfig) {
    let interval = Duration::from_secs(config.interval_hours * 3600);
    loop {
        sleep(interval).await;
        if let Err(err) = run_review_tick(&state, &config).await {
            tracing::error!("scheduler: periodic review tick failed: {err}");
        }
    }
}

async fn run_review_tick(state: &Arc<AppState>, config: &ReviewConfig) -> anyhow::Result<()> {
    let project_root = &state.core.project_root;

    // Query EventStore for the most recent "periodic_review" event timestamp.
    let events = state
        .observability
        .events
        .query(&EventFilters {
            hook: Some("periodic_review".to_string()),
            ..EventFilters::default()
        })
        .await
        .map_err(|e| anyhow::anyhow!("failed to query periodic_review events: {e}"))?;

    let last_review_ts = events.iter().map(|e| e.ts).max();

    // If there was a prior review, skip this cycle when no new commits have landed.
    // On first boot (no prior event) we always run.
    if let Some(ts) = last_review_ts {
        let since = ts.to_rfc3339();
        let output = tokio::process::Command::new("git")
            .args(["log", "--oneline", &format!("--since={since}"), "-1"])
            .current_dir(project_root)
            .output()
            .await
            .map_err(|e| anyhow::anyhow!("failed to run git log: {e}"))?;

        if String::from_utf8_lossy(&output.stdout).trim().is_empty() {
            tracing::debug!(
                since = %since,
                "scheduler: periodic review skipped — no new commits"
            );
            return Ok(());
        }
    }

    // Gather context strings for the review prompt.
    let since_arg = last_review_ts
        .map(|ts| ts.to_rfc3339())
        .unwrap_or_else(|| "1970-01-01T00:00:00Z".to_string());

    let repo_structure = gather_repo_structure(project_root).await;
    let diff_stat = gather_diff_stat(project_root, &since_arg).await;
    let recent_commits = gather_recent_commits(project_root, &since_arg).await;

    let prompt =
        harness_core::prompts::periodic_review_prompt(&repo_structure, &diff_stat, &recent_commits);

    let req = CreateTaskRequest {
        prompt: Some(prompt),
        agent: config.agent.clone(),
        turn_timeout_secs: config.timeout_secs,
        source: Some("periodic_review".to_string()),
        ..CreateTaskRequest::default()
    };

    match task_routes::enqueue_task(state, req).await {
        Ok(task_id) => {
            tracing::info!(
                task_id = %task_id,
                "scheduler: periodic review task enqueued"
            );
        }
        Err(err) => {
            return Err(anyhow::anyhow!(
                "failed to enqueue periodic review task: {err}"
            ));
        }
    }

    // Log a "periodic_review" event so the next cycle can check the timestamp.
    let event = Event::new(
        SessionId::new(),
        "periodic_review",
        "scheduler",
        Decision::Pass,
    );
    if let Err(err) = state.observability.events.log(&event).await {
        tracing::warn!("scheduler: failed to log periodic_review event: {err}");
    }

    Ok(())
}

async fn gather_repo_structure(project_root: &std::path::Path) -> String {
    let output = tokio::process::Command::new("git")
        .args(["ls-files", "--", "*.rs"])
        .current_dir(project_root)
        .output()
        .await;
    match output {
        Ok(o) => String::from_utf8_lossy(&o.stdout).into_owned(),
        Err(_) => String::new(),
    }
}

async fn gather_diff_stat(project_root: &std::path::Path, since: &str) -> String {
    // Find the first new commit after `since` to diff from its parent.
    let rev_output = tokio::process::Command::new("git")
        .args([
            "log",
            "--format=%H",
            &format!("--since={since}"),
            "--reverse",
            "-1",
        ])
        .current_dir(project_root)
        .output()
        .await;

    let first_new_commit = match rev_output {
        Ok(o) => {
            let hash = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if hash.is_empty() {
                return String::new();
            }
            hash
        }
        Err(_) => return String::new(),
    };

    let output = tokio::process::Command::new("git")
        .args(["diff", "--stat", &format!("{first_new_commit}^"), "HEAD"])
        .current_dir(project_root)
        .output()
        .await;

    match output {
        Ok(o) => String::from_utf8_lossy(&o.stdout).into_owned(),
        Err(_) => String::new(),
    }
}

async fn gather_recent_commits(project_root: &std::path::Path, since: &str) -> String {
    let output = tokio::process::Command::new("git")
        .args(["log", "--oneline", &format!("--since={since}")])
        .current_dir(project_root)
        .output()
        .await;
    match output {
        Ok(o) => String::from_utf8_lossy(&o.stdout).into_owned(),
        Err(_) => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn review_config_defaults_disabled() {
        let config = ReviewConfig::default();
        assert!(!config.enabled);
        assert_eq!(config.interval_hours, 24);
        assert_eq!(config.timeout_secs, 900);
        assert!(config.agent.is_none());
    }

    #[test]
    fn review_config_custom_values() {
        let config = ReviewConfig {
            enabled: true,
            interval_hours: 12,
            agent: Some("claude".to_string()),
            timeout_secs: 600,
        };
        assert!(config.enabled);
        assert_eq!(config.interval_hours, 12);
        assert_eq!(config.agent.as_deref(), Some("claude"));
        assert_eq!(config.timeout_secs, 600);
    }

    #[test]
    fn create_task_request_source_field() {
        let req = CreateTaskRequest {
            prompt: Some("review".to_string()),
            source: Some("periodic_review".to_string()),
            ..CreateTaskRequest::default()
        };
        assert_eq!(req.source.as_deref(), Some("periodic_review"));
    }

    #[test]
    fn create_task_request_source_defaults_to_none() {
        let req = CreateTaskRequest::default();
        assert!(req.source.is_none());
    }
}
