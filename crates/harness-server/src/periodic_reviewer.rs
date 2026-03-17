use crate::http::task_routes;
use crate::http::AppState;
use crate::task_runner::CreateTaskRequest;
use harness_core::{Decision, Event, EventFilters, ReviewConfig, ReviewMode, SessionId};
use std::path::Path;
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
    // Collect all active project roots to review. When the registry is available, iterate
    // every active project; otherwise fall back to the single default project root.
    let project_roots: Vec<(Option<String>, std::path::PathBuf)> =
        match &state.core.project_registry {
            Some(registry) => {
                let projects = registry
                    .list()
                    .await
                    .map_err(|e| anyhow::anyhow!("failed to list projects: {e}"))?;
                projects
                    .into_iter()
                    .filter(|p| p.active)
                    .map(|p| (Some(p.id), p.root))
                    .collect()
            }
            None => vec![(None, state.core.project_root.clone())],
        };

    if project_roots.is_empty() {
        tracing::debug!("scheduler: no active projects to review");
        return Ok(());
    }

    for (project_id, root) in project_roots {
        if let Err(err) = run_review_for_project(state, config, &root, project_id.as_deref()).await
        {
            tracing::error!(
                project = ?project_id,
                root = %root.display(),
                "scheduler: review failed for project: {err}"
            );
        }
    }
    Ok(())
}

/// Run one review cycle for a single project root.
///
/// `project_id` is used to scope the event-store checkpoint so each project
/// tracks its own last-review timestamp independently.
async fn run_review_for_project(
    state: &Arc<AppState>,
    config: &ReviewConfig,
    project_root: &Path,
    project_id: Option<&str>,
) -> anyhow::Result<()> {
    // Key the checkpoint by canonical repo root so that:
    // - the same physical repo always maps to one checkpoint regardless of how
    //   many project IDs point at it, and
    // - repointing an ID to a new root does not inherit the old root's checkpoint.
    let hook_name = format!("periodic_review:{}", project_root.to_string_lossy());

    // In incremental mode: skip the cycle when no new commits have landed.
    // In full mode: always run.
    if config.mode == ReviewMode::Incremental {
        let events = state
            .observability
            .events
            .query(&EventFilters {
                hook: Some(hook_name.clone()),
                ..EventFilters::default()
            })
            .await
            .map_err(|e| anyhow::anyhow!("failed to query {hook_name} events: {e}"))?;

        if let Some(ts) = events.iter().map(|e| e.ts).max() {
            let since = ts.to_rfc3339();
            let output = tokio::process::Command::new("git")
                .args(["log", "--oneline", &format!("--since={since}"), "-1"])
                .current_dir(project_root)
                .output()
                .await
                .map_err(|e| anyhow::anyhow!("failed to run git log: {e}"))?;

            if String::from_utf8_lossy(&output.stdout).trim().is_empty() {
                tracing::debug!(
                    project = ?project_id,
                    since = %since,
                    "scheduler: periodic review skipped — no new commits"
                );
                return Ok(());
            }
        }
    }

    let prompt = build_prompt(state, config, project_root, &hook_name).await?;

    let req = CreateTaskRequest {
        prompt: Some(prompt),
        agent: config.agent.clone(),
        turn_timeout_secs: config.timeout_secs,
        source: Some("periodic_review".to_string()),
        project: Some(project_root.to_path_buf()),
        ..CreateTaskRequest::default()
    };

    match task_routes::enqueue_task(state, req).await {
        Ok(task_id) => {
            tracing::info!(
                task_id = %task_id,
                project = ?project_id,
                mode = ?config.mode,
                "scheduler: periodic review task enqueued"
            );
        }
        Err(err) => {
            return Err(anyhow::anyhow!(
                "failed to enqueue periodic review task: {err}"
            ));
        }
    }

    // Log a checkpoint event so the next cycle can check the timestamp.
    let event = Event::new(SessionId::new(), &hook_name, "scheduler", Decision::Pass);
    if let Err(err) = state.observability.events.log(&event).await {
        tracing::warn!(
            project = ?project_id,
            "scheduler: failed to log {hook_name} event: {err}"
        );
    }

    Ok(())
}

/// Build the review prompt based on the configured mode.
///
/// - `full`: only repo structure, no diff/commit noise.
/// - `incremental`: repo structure + diff stat + recent commits.
async fn build_prompt(
    state: &Arc<AppState>,
    config: &ReviewConfig,
    project_root: &Path,
    hook_name: &str,
) -> anyhow::Result<String> {
    let repo_structure = gather_repo_structure(project_root).await;

    let prompt = if config.mode == ReviewMode::Full {
        harness_core::prompts::full_repo_review_prompt(&repo_structure)
    } else {
        // Incremental: compute the since timestamp from the last review event.
        let events = state
            .observability
            .events
            .query(&EventFilters {
                hook: Some(hook_name.to_string()),
                ..EventFilters::default()
            })
            .await
            .map_err(|e| anyhow::anyhow!("failed to query {hook_name} events: {e}"))?;
        let since_arg = events
            .iter()
            .map(|e| e.ts)
            .max()
            .map(|ts| ts.to_rfc3339())
            .unwrap_or_else(|| "1970-01-01T00:00:00Z".to_string());

        let diff_stat = gather_diff_stat(project_root, &since_arg).await;
        let recent_commits = gather_recent_commits(project_root, &since_arg).await;
        harness_core::prompts::periodic_review_prompt(&repo_structure, &diff_stat, &recent_commits)
    };

    Ok(prompt)
}

async fn gather_repo_structure(project_root: &Path) -> String {
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

async fn gather_diff_stat(project_root: &Path, since: &str) -> String {
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

async fn gather_recent_commits(project_root: &Path, since: &str) -> String {
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
        assert_eq!(config.mode, ReviewMode::Incremental);
    }

    #[test]
    fn review_config_custom_values() {
        let config = ReviewConfig {
            enabled: true,
            interval_hours: 12,
            agent: Some("claude".to_string()),
            timeout_secs: 600,
            mode: ReviewMode::Full,
        };
        assert!(config.enabled);
        assert_eq!(config.interval_hours, 12);
        assert_eq!(config.agent.as_deref(), Some("claude"));
        assert_eq!(config.timeout_secs, 600);
        assert_eq!(config.mode, ReviewMode::Full);
    }

    #[test]
    fn review_mode_default_is_incremental() {
        assert_eq!(ReviewMode::default(), ReviewMode::Incremental);
    }

    #[test]
    fn hook_name_keyed_by_root() {
        // Verify the hook naming logic used in run_review_for_project:
        // checkpoint key is derived from the canonical repo root, not the
        // project ID, so the same physical repo always maps to one checkpoint
        // regardless of how many IDs point at it.
        let root_a = std::path::Path::new("/home/user/repo-a");
        let root_b = std::path::Path::new("/home/user/repo-b");
        let name_a = format!("periodic_review:{}", root_a.to_string_lossy());
        let name_b = format!("periodic_review:{}", root_b.to_string_lossy());
        assert_ne!(name_a, name_b);
        assert_eq!(name_a, "periodic_review:/home/user/repo-a");

        // Same root with different hypothetical IDs must produce the same key.
        let name_a2 = format!("periodic_review:{}", root_a.to_string_lossy());
        assert_eq!(name_a, name_a2);
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
