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
    let interval = config.effective_interval();

    if config.run_on_startup {
        // Brief delay to let the server fully initialize before the first tick.
        sleep(Duration::from_secs(5)).await;
        if let Err(err) = run_review_tick(&state, &config).await {
            tracing::error!("scheduler: periodic review startup tick failed: {err}");
        }
    }

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

    // Run VibeGuard guard scans and include violations in the review context.
    let guard_violations = {
        let rules = state.engines.rules.read().await;
        match rules.scan(project_root).await {
            Ok(violations) => {
                if violations.is_empty() {
                    String::new()
                } else {
                    let mut buf = String::from("Guard scan found the following violations:\n");
                    for v in &violations {
                        buf.push_str(&format!(
                            "- [{}] {}: {}\n",
                            v.rule_id,
                            v.file.display(),
                            v.message
                        ));
                    }
                    buf
                }
            }
            Err(e) => {
                tracing::warn!("scheduler: guard scan failed, proceeding without: {e}");
                String::new()
            }
        }
    };

    // Gather existing review issues to prevent duplicate creation.
    let existing_issues = gather_existing_review_issues(project_root).await;

    let prompt = harness_core::prompts::periodic_review_prompt(
        &repo_structure,
        &diff_stat,
        &recent_commits,
        &guard_violations,
        &existing_issues,
    );

    let req = CreateTaskRequest {
        prompt: Some(prompt),
        agent: config.agent.clone(),
        turn_timeout_secs: config.timeout_secs,
        source: Some("periodic_review".to_string()),
        ..CreateTaskRequest::default()
    };

    let task_id = match task_routes::enqueue_task(state, req).await {
        Ok(id) => {
            tracing::info!(task_id = %id, "scheduler: periodic review task enqueued");
            id
        }
        Err(err) => {
            return Err(anyhow::anyhow!(
                "failed to enqueue periodic review task: {err}"
            ));
        }
    };

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

    // Wait for the review task to complete, then persist structured findings.
    let store = state.core.tasks.clone();
    let review_store = state.observability.review_store.clone();
    let timeout_secs = config.timeout_secs;
    tokio::spawn(async move {
        let poll_interval = Duration::from_secs(15);
        let max_wait = Duration::from_secs(timeout_secs + 120);
        let start = tokio::time::Instant::now();
        loop {
            sleep(poll_interval).await;
            if start.elapsed() > max_wait {
                tracing::warn!(task_id = %task_id, "scheduler: review task polling timed out");
                break;
            }
            let Some(task) = store.get(&task_id) else {
                continue;
            };
            if !matches!(
                task.status,
                crate::task_runner::TaskStatus::Done | crate::task_runner::TaskStatus::Failed
            ) {
                continue;
            }
            // Collect agent output from round details.
            let output: String = task
                .rounds
                .iter()
                .filter_map(|r| r.detail.as_deref())
                .collect::<Vec<_>>()
                .join("\n");
            if output.is_empty() {
                tracing::warn!(task_id = %task_id, "scheduler: review task completed but no output");
                break;
            }
            match crate::review_store::parse_review_output(&output) {
                Ok(review) => {
                    tracing::info!(
                        task_id = %task_id,
                        findings = review.findings.len(),
                        health_score = review.summary.health_score,
                        "scheduler: periodic review parsed"
                    );
                    if let Some(ref rs) = review_store {
                        match rs.persist_findings(&task_id.0, &review.findings).await {
                            Ok(n) => tracing::info!(
                                new_findings = n,
                                "scheduler: review findings persisted"
                            ),
                            Err(e) => tracing::warn!("scheduler: failed to persist findings: {e}"),
                        }
                    }
                    // Issue creation is handled by the review agent itself
                    // (instructed in the prompt to create GitHub issues for P0/P1 findings).
                }
                Err(e) => {
                    tracing::warn!(
                        task_id = %task_id,
                        "scheduler: failed to parse review output as JSON: {e}"
                    );
                }
            }
            break;
        }
    });

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

/// Fetch open GitHub issues with the "review" label so the agent can skip known problems.
async fn gather_existing_review_issues(project_root: &std::path::Path) -> String {
    let output = tokio::process::Command::new("gh")
        .args([
            "issue",
            "list",
            "--label",
            "review",
            "--state",
            "open",
            "--json",
            "number,title",
            "--jq",
            ".[] | \"#\\(.number) \\(.title)\"",
        ])
        .current_dir(project_root)
        .output()
        .await;
    match output {
        Ok(o) if o.status.success() => {
            let text = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if text.is_empty() {
                String::new()
            } else {
                text
            }
        }
        _ => String::new(),
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
        assert!(!config.run_on_startup);
        assert_eq!(config.interval_hours, 24);
        assert!(config.interval_secs.is_none());
        assert_eq!(config.timeout_secs, 900);
        assert!(config.agent.is_none());
    }

    #[test]
    fn review_config_custom_values() {
        let config = ReviewConfig {
            enabled: true,
            run_on_startup: true,
            interval_hours: 12,
            interval_secs: None,
            agent: Some("claude".to_string()),
            timeout_secs: 600,
        };
        assert!(config.enabled);
        assert!(config.run_on_startup);
        assert_eq!(config.interval_hours, 12);
        assert_eq!(config.agent.as_deref(), Some("claude"));
        assert_eq!(config.timeout_secs, 600);
    }

    #[test]
    fn effective_interval_prefers_secs_over_hours() {
        let config = ReviewConfig {
            interval_hours: 24,
            interval_secs: Some(300),
            ..ReviewConfig::default()
        };
        assert_eq!(config.effective_interval(), Duration::from_secs(300));
    }

    #[test]
    fn effective_interval_falls_back_to_hours() {
        let config = ReviewConfig {
            interval_hours: 2,
            interval_secs: None,
            ..ReviewConfig::default()
        };
        assert_eq!(config.effective_interval(), Duration::from_secs(7200));
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
