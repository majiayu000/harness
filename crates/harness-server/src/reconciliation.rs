use crate::http::AppState;
use crate::task_runner::{mutate_and_persist, TaskId, TaskStatus, TaskStore};
use harness_core::config::misc::ReconciliationConfig;
use harness_workflow::issue_lifecycle::IssueWorkflowStore;
use harness_workflow::runtime::{
    DecisionValidator, ValidationContext, WorkflowCommand, WorkflowCommandStatus,
    WorkflowCommandType, WorkflowDecision, WorkflowDecisionTransition, WorkflowEvidence,
    WorkflowInstance, WorkflowRuntimeStore, GITHUB_ISSUE_PR_DEFINITION_ID,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::time::sleep;

#[path = "reconciliation_apply.rs"]
mod reconciliation_apply;
#[path = "reconciliation_github.rs"]
mod reconciliation_github;
#[path = "reconciliation_legacy.rs"]
mod reconciliation_legacy;
#[path = "reconciliation_periodic.rs"]
mod reconciliation_periodic;
#[path = "reconciliation_runtime.rs"]
mod reconciliation_runtime;

use self::reconciliation_apply::apply_runtime_workflow_transition;
use self::reconciliation_github::fetch_pr_state_by_url;
#[cfg(test)]
use self::reconciliation_github::{
    classify_issue_state, classify_pr_state, GitHubIssueState, GitHubPullState,
};
pub(crate) use self::reconciliation_github::{
    fetch_issue_state_with_token, fetch_pr_state_by_slug_with_token, github_api_base_url,
    GitHubState,
};
use self::reconciliation_legacy::{apply_transition, resolve_github_state};
#[cfg(test)]
use self::reconciliation_runtime::runtime_candidate_from_instance;
use self::reconciliation_runtime::{collect_runtime_candidates, resolve_runtime_github_state};
pub use reconciliation_periodic::start;

/// One candidate task for reconciliation check.
struct Candidate {
    id: TaskId,
    pr_url: Option<String>,
    repo: Option<String>,
    project_root: Option<PathBuf>,
    /// Numeric issue or PR from `external_id` (e.g. `issue:42` → 42).
    issue_num: Option<u64>,
    /// Numeric PR from `external_id` `pr:N` when no `pr_url` is present.
    pr_num_from_ext: Option<u64>,
}

/// A workflow-runtime issue PR workflow with a bound GitHub PR or issue.
struct RuntimeWorkflowCandidate {
    workflow_id: String,
    state: String,
    row_updated_at: chrono::DateTime<chrono::Utc>,
    repo: Option<String>,
    project_root: Option<PathBuf>,
    issue_number: Option<u64>,
    pr_number: Option<u64>,
    pr_url: Option<String>,
}

#[derive(Debug, Clone, Copy)]
struct RuntimeWorkflowReconciliationSettings {
    ready_to_merge_min_age_secs: u64,
    ready_to_merge_alert_ttl_secs: u64,
}

impl RuntimeWorkflowReconciliationSettings {
    fn from_config(config: &ReconciliationConfig) -> Self {
        Self {
            ready_to_merge_min_age_secs: config.ready_to_merge_min_age_secs,
            ready_to_merge_alert_ttl_secs: config.ready_to_merge_alert_ttl_secs,
        }
    }
}

impl Default for RuntimeWorkflowReconciliationSettings {
    fn default() -> Self {
        Self::from_config(&ReconciliationConfig::default())
    }
}

/// A single resolved transition produced by `run_once`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReconciliationTransition {
    pub task_id: String,
    pub from: String,
    pub to: String,
    pub reason: String,
    /// `false` in dry-run mode.
    pub applied: bool,
}

/// A workflow-runtime transition produced by reconciliation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowReconciliationTransition {
    pub workflow_id: String,
    pub from: String,
    pub to: String,
    pub reason: String,
    pub applied: bool,
    pub repo: Option<String>,
    pub issue_number: Option<u64>,
    pub pr_number: Option<u64>,
    pub pr_url: Option<String>,
}

/// A workflow-runtime condition that reconciliation observed but did not transition.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowReconciliationAlert {
    pub workflow_id: String,
    pub state: String,
    pub reason: String,
    pub age_secs: u64,
    pub ttl_secs: u64,
    pub repo: Option<String>,
    pub issue_number: Option<u64>,
    pub pr_number: Option<u64>,
    pub pr_url: Option<String>,
}

/// Summary returned by `run_once` and serialised in the HTTP handler.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReconciliationReport {
    pub candidates: usize,
    pub skipped_terminal: usize,
    pub transitions: Vec<ReconciliationTransition>,
    #[serde(default)]
    pub workflow_transitions: Vec<WorkflowReconciliationTransition>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub workflow_alerts: Vec<WorkflowReconciliationAlert>,
}

/// Rate-limit state: at most `max_per_minute` GitHub API calls per 60-second window.
struct RateLimiter {
    max_per_minute: u32,
    calls_this_window: u32,
    window_start: Instant,
}

impl RateLimiter {
    fn new(max_per_minute: u32) -> Self {
        Self {
            max_per_minute,
            calls_this_window: 0,
            window_start: Instant::now(),
        }
    }

    /// Wait if the current window is exhausted, then record one call.
    async fn acquire(&mut self) {
        if self.max_per_minute == 0 {
            return;
        }
        let elapsed = self.window_start.elapsed();
        if elapsed >= Duration::from_secs(60) {
            self.window_start = Instant::now();
            self.calls_this_window = 0;
        }
        if self.calls_this_window >= self.max_per_minute {
            let remaining = Duration::from_secs(60).saturating_sub(self.window_start.elapsed());
            if !remaining.is_zero() {
                sleep(remaining).await;
            }
            self.window_start = Instant::now();
            self.calls_this_window = 0;
        }
        self.calls_this_window += 1;
    }
}

/// Try to build a `Candidate` from one `TaskState`.
///
/// Returns `None` when the task is already terminal or has no GitHub reference.
fn candidate_from_task(task: &crate::task_runner::TaskState) -> Option<Candidate> {
    if task.status.is_terminal() {
        return None;
    }
    let (issue_num, pr_num_from_ext) = parse_external_id(task.external_id.as_deref());
    let has_pr = task.pr_url.is_some() || pr_num_from_ext.is_some();
    let has_issue = issue_num.is_some();
    if !has_pr && !has_issue {
        return None;
    }
    Some(Candidate {
        id: task.id.clone(),
        pr_url: task.pr_url.clone(),
        repo: task.repo.clone(),
        project_root: task.project_root.clone(),
        issue_num,
        pr_num_from_ext,
    })
}

/// Collect non-terminal tasks that have a `pr_url` or a parseable `external_id`.
fn collect_candidates(store: &TaskStore) -> (Vec<Candidate>, usize) {
    let mut candidates = Vec::new();
    let mut skipped_terminal = 0usize;

    for entry in store.cache.iter() {
        let task = entry.value();
        if task.status.is_terminal() {
            skipped_terminal += 1;
            continue;
        }
        if let Some(c) = candidate_from_task(task) {
            candidates.push(c);
        }
    }
    (candidates, skipped_terminal)
}

fn optional_json_string(data: &serde_json::Value, key: &str) -> Option<String> {
    data.get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

pub(crate) fn parse_external_id(eid: Option<&str>) -> (Option<u64>, Option<u64>) {
    match eid {
        Some(s) if s.starts_with("issue:") => (s["issue:".len()..].parse().ok(), None),
        Some(s) if s.starts_with("pr:") => (None, s["pr:".len()..].parse().ok()),
        _ => (None, None),
    }
}

/// Core reconciliation logic. Callable from the periodic loop and HTTP handler.
pub async fn run_once(
    store: &Arc<TaskStore>,
    max_gh_calls_per_minute: u32,
    dry_run: bool,
) -> ReconciliationReport {
    run_once_with_token(store, max_gh_calls_per_minute, dry_run, None).await
}

pub async fn run_once_with_token(
    store: &Arc<TaskStore>,
    max_gh_calls_per_minute: u32,
    dry_run: bool,
    github_token: Option<&str>,
) -> ReconciliationReport {
    run_once_with_runtime_token(
        store,
        None,
        None,
        max_gh_calls_per_minute,
        dry_run,
        github_token,
    )
    .await
}

pub async fn run_once_with_runtime_token(
    store: &Arc<TaskStore>,
    runtime_store: Option<&WorkflowRuntimeStore>,
    issue_workflows: Option<&IssueWorkflowStore>,
    max_gh_calls_per_minute: u32,
    dry_run: bool,
    github_token: Option<&str>,
) -> ReconciliationReport {
    run_once_with_runtime_settings(
        store,
        runtime_store,
        issue_workflows,
        max_gh_calls_per_minute,
        RuntimeWorkflowReconciliationSettings::default(),
        dry_run,
        github_token,
    )
    .await
}

pub async fn run_once_with_runtime_config(
    store: &Arc<TaskStore>,
    runtime_store: Option<&WorkflowRuntimeStore>,
    issue_workflows: Option<&IssueWorkflowStore>,
    config: &ReconciliationConfig,
    dry_run: bool,
    github_token: Option<&str>,
) -> ReconciliationReport {
    run_once_with_runtime_settings(
        store,
        runtime_store,
        issue_workflows,
        config.max_gh_calls_per_minute,
        RuntimeWorkflowReconciliationSettings::from_config(config),
        dry_run,
        github_token,
    )
    .await
}

async fn run_once_with_runtime_settings(
    store: &Arc<TaskStore>,
    runtime_store: Option<&WorkflowRuntimeStore>,
    issue_workflows: Option<&IssueWorkflowStore>,
    max_gh_calls_per_minute: u32,
    runtime_settings: RuntimeWorkflowReconciliationSettings,
    dry_run: bool,
    github_token: Option<&str>,
) -> ReconciliationReport {
    let (candidates, skipped_terminal) = collect_candidates(store);
    let mut rate = RateLimiter::new(max_gh_calls_per_minute);
    let mut repo_slug_cache = HashMap::new();
    let mut transitions = Vec::new();
    let mut workflow_transitions = Vec::new();
    let mut workflow_alerts = Vec::new();
    let mut runtime_candidate_count = 0usize;
    let mut runtime_skipped_terminal = 0usize;

    for candidate in &candidates {
        let gh_state =
            resolve_github_state(candidate, &mut rate, &mut repo_slug_cache, github_token).await;

        let new_status = transition_for_github_state(gh_state);

        let Some((target_status, reason)) = new_status else {
            continue;
        };

        // Get current status for the transition record.
        let from_status = store
            .cache
            .get(&candidate.id)
            .map(|e| e.status.as_ref().to_string())
            .unwrap_or_else(|| "unknown".to_string());

        let applied = if dry_run {
            false
        } else {
            apply_transition(store, &candidate.id, target_status.clone(), reason).await
        };

        if !dry_run && applied {
            store.abort_task(&candidate.id);
        }

        transitions.push(ReconciliationTransition {
            task_id: candidate.id.0.clone(),
            from: from_status,
            to: target_status.as_ref().to_string(),
            reason: reason.to_string(),
            applied,
        });
    }

    if let Some(runtime_store) = runtime_store {
        match run_runtime_workflow_reconciliation_once(
            runtime_store,
            issue_workflows,
            &mut rate,
            runtime_settings,
            dry_run,
            github_token,
        )
        .await
        {
            Ok((candidate_count, skipped, transitions, alerts)) => {
                runtime_candidate_count = candidate_count;
                runtime_skipped_terminal = skipped;
                workflow_transitions = transitions;
                workflow_alerts = alerts;
            }
            Err(error) => {
                tracing::warn!("workflow runtime reconciliation failed: {error}");
            }
        }
    }

    let total_candidates =
        candidates.len() + runtime_candidate_count + skipped_terminal + runtime_skipped_terminal;
    ReconciliationReport {
        candidates: total_candidates,
        skipped_terminal: skipped_terminal + runtime_skipped_terminal,
        transitions,
        workflow_transitions,
        workflow_alerts,
    }
}

fn transition_for_github_state(gh_state: GitHubState) -> Option<(TaskStatus, &'static str)> {
    match gh_state {
        GitHubState::PrMerged => Some((TaskStatus::Done, "reconciled: PR merged externally")),
        GitHubState::PrClosed => Some((TaskStatus::Cancelled, "reconciled: PR closed externally")),
        GitHubState::IssueCompleted => {
            Some((TaskStatus::Done, "reconciled: issue completed externally"))
        }
        GitHubState::IssueClosed => {
            Some((TaskStatus::Cancelled, "reconciled: issue closed before PR"))
        }
        GitHubState::Open | GitHubState::Unknown => None,
    }
}

fn runtime_transition_for_github_state(
    gh_state: GitHubState,
) -> Option<(&'static str, &'static str)> {
    match gh_state {
        GitHubState::PrMerged => Some(("done", "reconciled: PR merged externally")),
        GitHubState::PrClosed => Some(("cancelled", "reconciled: PR closed externally")),
        GitHubState::IssueCompleted => Some(("done", "reconciled: issue completed externally")),
        GitHubState::IssueClosed => Some(("cancelled", "reconciled: issue closed externally")),
        GitHubState::Open | GitHubState::Unknown => None,
    }
}

async fn run_runtime_workflow_reconciliation_once(
    runtime_store: &WorkflowRuntimeStore,
    issue_workflows: Option<&IssueWorkflowStore>,
    rate: &mut RateLimiter,
    settings: RuntimeWorkflowReconciliationSettings,
    dry_run: bool,
    github_token: Option<&str>,
) -> anyhow::Result<(
    usize,
    usize,
    Vec<WorkflowReconciliationTransition>,
    Vec<WorkflowReconciliationAlert>,
)> {
    let (candidates, skipped_terminal) = collect_runtime_candidates(runtime_store).await?;
    let mut transitions = Vec::new();
    let mut alerts = Vec::new();
    let now = chrono::Utc::now();

    for candidate in &candidates {
        if candidate.state == "ready_to_merge"
            && runtime_candidate_age_secs(candidate, now) < settings.ready_to_merge_min_age_secs
        {
            continue;
        }
        let gh_state = resolve_runtime_github_state(candidate, rate, github_token).await;
        if let Some(alert) = ready_to_merge_open_alert(candidate, gh_state, settings, now) {
            alerts.push(alert);
            continue;
        }
        let Some((target_state, reason)) = runtime_transition_for_github_state(gh_state) else {
            continue;
        };
        let applied = if dry_run {
            false
        } else {
            match apply_runtime_workflow_transition(
                runtime_store,
                issue_workflows,
                candidate,
                target_state,
                reason,
            )
            .await
            {
                Ok(applied) => applied,
                Err(error) => {
                    tracing::warn!(
                        workflow_id = %candidate.workflow_id,
                        pr = candidate.pr_number,
                        repo = candidate.repo.as_deref(),
                        "workflow runtime reconciliation transition failed: {error}"
                    );
                    false
                }
            }
        };
        transitions.push(WorkflowReconciliationTransition {
            workflow_id: candidate.workflow_id.clone(),
            from: candidate.state.clone(),
            to: target_state.to_string(),
            reason: reason.to_string(),
            applied,
            repo: candidate.repo.clone(),
            issue_number: candidate.issue_number,
            pr_number: candidate.pr_number,
            pr_url: candidate.pr_url.clone(),
        });
    }

    Ok((candidates.len(), skipped_terminal, transitions, alerts))
}

fn runtime_candidate_age_secs(
    candidate: &RuntimeWorkflowCandidate,
    now: chrono::DateTime<chrono::Utc>,
) -> u64 {
    now.signed_duration_since(candidate.row_updated_at)
        .num_seconds()
        .max(0) as u64
}

fn ready_to_merge_open_alert(
    candidate: &RuntimeWorkflowCandidate,
    gh_state: GitHubState,
    settings: RuntimeWorkflowReconciliationSettings,
    now: chrono::DateTime<chrono::Utc>,
) -> Option<WorkflowReconciliationAlert> {
    if candidate.state != "ready_to_merge" || gh_state != GitHubState::Open {
        return None;
    }
    let age_secs = runtime_candidate_age_secs(candidate, now);
    if age_secs < settings.ready_to_merge_alert_ttl_secs {
        return None;
    }
    Some(WorkflowReconciliationAlert {
        workflow_id: candidate.workflow_id.clone(),
        state: candidate.state.clone(),
        reason: "ready_to_merge PR remains open past reconciliation alert TTL".to_string(),
        age_secs,
        ttl_secs: settings.ready_to_merge_alert_ttl_secs,
        repo: candidate.repo.clone(),
        issue_number: candidate.issue_number,
        pr_number: candidate.pr_number,
        pr_url: candidate.pr_url.clone(),
    })
}

#[cfg(test)]
#[path = "reconciliation_payload_tests.rs"]
mod payload_tests;
#[cfg(test)]
#[path = "reconciliation_state_tests.rs"]
mod state_tests;
#[cfg(test)]
#[path = "reconciliation_tests.rs"]
mod tests;
