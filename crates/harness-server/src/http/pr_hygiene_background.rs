use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;

use harness_core::types::{Decision, Event, SessionId};

use super::{background, state::AppState};

const RUNTIME_WORKFLOW_CONFIG_RETRY_SECS: u64 = 30;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(super) struct RuntimePrHygieneSweepTick {
    pub inspected: usize,
    pub repair_requested: usize,
    pub active_command_exists: usize,
    pub not_needed: usize,
    pub skipped: usize,
    pub rejected: usize,
}

impl RuntimePrHygieneSweepTick {
    fn has_visible_activity(self) -> bool {
        self.inspected > 0
            || self.repair_requested > 0
            || self.active_command_exists > 0
            || self.not_needed > 0
            || self.skipped > 0
            || self.rejected > 0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct RuntimePrHygieneRepo {
    repo: String,
    project_root: PathBuf,
}

pub(super) async fn run_runtime_pr_hygiene_sweep_tick(
    state: &Arc<AppState>,
    limit: usize,
) -> anyhow::Result<RuntimePrHygieneSweepTick> {
    let Some(store) = state.core.workflow_runtime_store.as_ref() else {
        return Ok(RuntimePrHygieneSweepTick::default());
    };
    let repos = runtime_pr_hygiene_repos(state).await;
    let mut tick = RuntimePrHygieneSweepTick::default();
    let mut remaining = limit.max(1);
    for repo in repos {
        if remaining == 0 {
            break;
        }
        if !repo.project_root.exists() {
            tracing::warn!(
                repo = %repo.repo,
                project_root = %repo.project_root.display(),
                "workflow runtime PR hygiene skipped unresolvable project path"
            );
            tick.skipped += 1;
            continue;
        }
        let workflow_cfg = match background::load_runtime_workflow_config(
            &repo.project_root,
            "workflow runtime PR hygiene",
        ) {
            Ok(config) => config,
            Err(_) => {
                tick.skipped += 1;
                continue;
            }
        };
        if !workflow_cfg.pr_feedback.enabled
            || !workflow_cfg.pr_feedback.hygiene_enabled
            || !workflow_cfg.runtime_dispatch.enabled
            || !workflow_cfg.runtime_worker.enabled
        {
            tick.skipped += 1;
            continue;
        }
        let repo_slug = workflow_source_repo(&workflow_cfg)
            .unwrap_or(repo.repo.as_str())
            .to_string();
        let fetch_limit =
            remaining.min(workflow_cfg.pr_feedback.hygiene_batch_limit.max(1) as usize);
        let open_prs = crate::github_pr_hygiene::fetch_open_pr_hygiene(
            &repo_slug,
            state.core.server.config.server.github_token.as_deref(),
            fetch_limit,
        )
        .await?;
        let now = chrono::Utc::now();
        for pr in open_prs {
            if remaining == 0 {
                break;
            }
            remaining -= 1;
            tick.inspected += 1;
            let dirty_age_secs = pr.dirty_age_secs(now);
            let result = if pr.requires_mergeability_repair()
                && should_request_pr_hygiene_repair(dirty_age_secs, &workflow_cfg.pr_feedback)
            {
                let task_id = crate::workflow_runtime_pr_feedback::synthesized_pr_feedback_task_id(
                    &repo.project_root.to_string_lossy(),
                    Some(pr.repo_slug.as_str()),
                    pr.pr_number,
                );
                let observed_at = now.to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
                let updated_at = pr.updated_at_rfc3339();
                let outcome = crate::workflow_runtime_pr_feedback::request_pr_hygiene_repair(
                    store,
                    crate::workflow_runtime_pr_feedback::PrHygieneRepairRuntimeContext {
                        project_root: &repo.project_root,
                        repo: Some(pr.repo_slug.as_str()),
                        task_id: &task_id,
                        pr_number: pr.pr_number,
                        pr_url: Some(pr.pr_url.as_str()),
                        title: pr.title.as_deref(),
                        merge_state_status: pr.merge_state_status.as_deref(),
                        head_oid: pr.head_oid.as_deref(),
                        updated_at: Some(updated_at.as_str()),
                        observed_at: observed_at.as_str(),
                        dirty_age_secs,
                        dirty_age_to_repair_secs: workflow_cfg.pr_feedback.dirty_age_to_repair_secs,
                        dirty_age_to_comment_secs: workflow_cfg
                            .pr_feedback
                            .dirty_age_to_comment_secs,
                        rebase_needed_label: workflow_cfg.pr_feedback.rebase_needed_label.as_str(),
                    },
                )
                .await?;
                map_repair_outcome(
                    &mut tick,
                    outcome,
                    dirty_age_secs,
                    &workflow_cfg.pr_feedback,
                )
            } else if pr.requires_mergeability_repair() {
                tick.not_needed += 1;
                "not_stale_enough"
            } else {
                tick.not_needed += 1;
                "ok"
            };
            record_pr_hygiene_telemetry(
                state,
                &pr,
                dirty_age_secs,
                result,
                &workflow_cfg.pr_feedback,
            )
            .await;
        }
    }
    Ok(tick)
}

fn should_request_pr_hygiene_repair(
    dirty_age_secs: u64,
    policy: &harness_core::config::workflow::PrFeedbackPolicy,
) -> bool {
    dirty_age_secs >= policy.dirty_age_to_repair_secs
}

fn map_repair_outcome(
    tick: &mut RuntimePrHygieneSweepTick,
    outcome: crate::workflow_runtime_pr_feedback::PrFeedbackSweepRequestOutcome,
    dirty_age_secs: u64,
    policy: &harness_core::config::workflow::PrFeedbackPolicy,
) -> &'static str {
    match outcome {
        crate::workflow_runtime_pr_feedback::PrFeedbackSweepRequestOutcome::Requested { .. } => {
            tick.repair_requested += 1;
            if dirty_age_secs >= policy.dirty_age_to_comment_secs {
                "comment_only"
            } else {
                "skill_dispatched"
            }
        }
        crate::workflow_runtime_pr_feedback::PrFeedbackSweepRequestOutcome::ActiveCommandExists {
            ..
        } => {
            tick.active_command_exists += 1;
            "active_command_exists"
        }
        crate::workflow_runtime_pr_feedback::PrFeedbackSweepRequestOutcome::NotCandidate {
            ..
        } => {
            tick.skipped += 1;
            "not_candidate"
        }
        crate::workflow_runtime_pr_feedback::PrFeedbackSweepRequestOutcome::Rejected { .. } => {
            tick.rejected += 1;
            "rejected"
        }
    }
}

async fn runtime_pr_hygiene_repos(state: &Arc<AppState>) -> Vec<RuntimePrHygieneRepo> {
    let mut repos = Vec::new();
    let mut seen = BTreeSet::new();
    if let Some(github_config) = state
        .core
        .server
        .config
        .intake
        .github
        .as_ref()
        .filter(|config| config.enabled)
    {
        for repo_config in github_config.effective_repos() {
            let project_root =
                background::github_repo_project_root(&repo_config, &state.core.project_root).await;
            push_pr_hygiene_repo(&mut repos, &mut seen, repo_config.repo, project_root);
        }
    }

    if let Ok(workflow_cfg) = background::load_runtime_workflow_config(
        &state.core.project_root,
        "workflow runtime PR hygiene",
    ) {
        if let Some(repo) = workflow_source_repo(&workflow_cfg) {
            push_pr_hygiene_repo(
                &mut repos,
                &mut seen,
                repo.to_string(),
                state.core.project_root.clone(),
            );
        }
    }
    repos
}

fn push_pr_hygiene_repo(
    repos: &mut Vec<RuntimePrHygieneRepo>,
    seen: &mut BTreeSet<(String, PathBuf)>,
    repo: String,
    project_root: PathBuf,
) {
    if repo.trim().is_empty() {
        return;
    }
    let key = (repo.clone(), project_root.clone());
    if seen.insert(key) {
        repos.push(RuntimePrHygieneRepo { repo, project_root });
    }
}

fn workflow_source_repo(
    workflow_cfg: &harness_core::config::workflow::WorkflowConfig,
) -> Option<&str> {
    workflow_cfg
        .source
        .kind
        .as_deref()
        .map(|kind| kind.eq_ignore_ascii_case("github"))
        .unwrap_or(true)
        .then_some(workflow_cfg.source.repo.as_deref())
        .flatten()
        .map(str::trim)
        .filter(|repo| !repo.is_empty())
}

async fn record_pr_hygiene_telemetry(
    state: &Arc<AppState>,
    pr: &crate::github_pr_hygiene::GitHubOpenPrHygiene,
    dirty_age_secs: u64,
    result: &str,
    policy: &harness_core::config::workflow::PrFeedbackPolicy,
) {
    let decision = match result {
        "rejected" | "not_candidate" | "active_command_exists" => Decision::Warn,
        _ => Decision::Pass,
    };
    let mut event = Event::new(
        SessionId::new(),
        "pr_rebase_attempted",
        "workflow_runtime_pr_hygiene",
        decision,
    );
    event.reason = Some(result.to_string());
    event.detail = Some(format!(
        "repo={} pr_number={} merge_state_status={} dirty_age_secs={} result={}",
        pr.repo_slug,
        pr.pr_number,
        pr.merge_state_status.as_deref().unwrap_or("<unknown>"),
        dirty_age_secs,
        result
    ));
    event.content = Some(
        serde_json::json!({
            "repo": pr.repo_slug,
            "pr_number": pr.pr_number,
            "pr_url": pr.pr_url,
            "merge_state_status": pr.merge_state_status,
            "head_oid": pr.head_oid,
            "updated_at": pr.updated_at_rfc3339(),
            "dirty_age_secs": dirty_age_secs,
            "age_source": "updated_at",
            "result": result,
            "dirty_age_to_repair_secs": policy.dirty_age_to_repair_secs,
            "dirty_age_to_comment_secs": policy.dirty_age_to_comment_secs,
            "rebase_needed_label": policy.rebase_needed_label,
        })
        .to_string(),
    );
    if let Err(error) = state.observability.events.log(&event).await {
        tracing::warn!(
            repo = %pr.repo_slug,
            pr_number = pr.pr_number,
            "workflow runtime PR hygiene failed to log pr_rebase_attempted: {error}"
        );
    }
    tracing::info!(
        repo = %pr.repo_slug,
        pr_number = pr.pr_number,
        merge_state_status = ?pr.merge_state_status,
        dirty_age_secs,
        result,
        "pr_rebase_attempted"
    );
}

pub(super) fn spawn_runtime_pr_hygiene_sweeper(state: &Arc<AppState>) {
    if state.core.workflow_runtime_store.is_none() {
        tracing::debug!("workflow runtime PR hygiene sweeper disabled: store unavailable");
        return;
    }

    let weak_state = Arc::downgrade(state);
    tokio::spawn(async move {
        loop {
            let state = match weak_state.upgrade() {
                Some(state) => state,
                None => break,
            };
            let workflow_cfg = match background::load_runtime_workflow_config(
                &state.core.project_root,
                "workflow runtime PR hygiene sweeper",
            ) {
                Ok(config) => config,
                Err(_) => {
                    tokio::time::sleep(std::time::Duration::from_secs(
                        RUNTIME_WORKFLOW_CONFIG_RETRY_SECS,
                    ))
                    .await;
                    continue;
                }
            };
            let interval = std::time::Duration::from_secs(
                workflow_cfg.pr_feedback.hygiene_interval_secs.max(1),
            );
            let batch_limit = workflow_cfg.pr_feedback.hygiene_batch_limit.max(1) as usize;
            if workflow_cfg.pr_feedback.enabled && workflow_cfg.pr_feedback.hygiene_enabled {
                match run_runtime_pr_hygiene_sweep_tick(&state, batch_limit).await {
                    Ok(tick) if tick.has_visible_activity() => {
                        tracing::info!(
                            inspected = tick.inspected,
                            repair_requested = tick.repair_requested,
                            active_command_exists = tick.active_command_exists,
                            not_needed = tick.not_needed,
                            skipped = tick.skipped,
                            rejected = tick.rejected,
                            "workflow runtime PR hygiene sweeper tick complete"
                        );
                    }
                    Ok(_) => {}
                    Err(error) => {
                        tracing::warn!("workflow runtime PR hygiene sweeper tick failed: {error}");
                    }
                }
            }
            tokio::time::sleep(interval).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workflow_source_repo_accepts_github_source_repo() {
        let mut cfg = harness_core::config::workflow::WorkflowConfig::default();
        cfg.source.kind = Some("github".to_string());
        cfg.source.repo = Some("owner/repo".to_string());

        assert_eq!(workflow_source_repo(&cfg), Some("owner/repo"));
    }

    #[test]
    fn workflow_source_repo_ignores_non_github_source_repo() {
        let mut cfg = harness_core::config::workflow::WorkflowConfig::default();
        cfg.source.kind = Some("feishu".to_string());
        cfg.source.repo = Some("owner/repo".to_string());

        assert_eq!(workflow_source_repo(&cfg), None);
    }

    #[test]
    fn map_repair_outcome_marks_comment_threshold() {
        let mut tick = RuntimePrHygieneSweepTick::default();
        let policy = harness_core::config::workflow::PrFeedbackPolicy {
            dirty_age_to_comment_secs: 100,
            ..Default::default()
        };

        let result = map_repair_outcome(
            &mut tick,
            crate::workflow_runtime_pr_feedback::PrFeedbackSweepRequestOutcome::Requested {
                workflow_id: "wf".to_string(),
                task_id: "task".to_string(),
            },
            100,
            &policy,
        );

        assert_eq!(result, "comment_only");
        assert_eq!(tick.repair_requested, 1);
    }

    #[test]
    fn should_request_pr_hygiene_repair_honors_repair_threshold() {
        let policy = harness_core::config::workflow::PrFeedbackPolicy {
            dirty_age_to_repair_secs: 100,
            ..Default::default()
        };

        assert!(!should_request_pr_hygiene_repair(99, &policy));
        assert!(should_request_pr_hygiene_repair(100, &policy));
    }
}
