use crate::http::AppState;
use harness_core::config::misc::ReconciliationConfig;
use harness_workflow::issue_lifecycle::IssueWorkflowStore;
use harness_workflow::runtime::{
    DecisionValidator, ValidationContext, WorkflowCommand, WorkflowCommandStatus,
    WorkflowCommandType, WorkflowDecision, WorkflowDecisionTransition, WorkflowEvidence,
    WorkflowInstance, WorkflowRuntimeStore, GITHUB_ISSUE_PR_DEFINITION_ID,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::time::sleep;

#[path = "reconciliation_github.rs"]
mod reconciliation_github;
#[path = "reconciliation_periodic.rs"]
mod reconciliation_periodic;
#[path = "reconciliation_runtime.rs"]
mod reconciliation_runtime;

use self::reconciliation_github::fetch_pr_state_by_url;
#[cfg(test)]
use self::reconciliation_github::{
    classify_issue_state, classify_pr_state, GitHubIssueState, GitHubPullState,
};
pub(crate) use self::reconciliation_github::{
    fetch_issue_state_with_token, fetch_pr_state_by_slug_with_token, github_api_base_url,
    GitHubState,
};
#[cfg(test)]
use self::reconciliation_runtime::runtime_candidate_from_instance;
use self::reconciliation_runtime::{collect_runtime_candidates, resolve_runtime_github_state};
pub use reconciliation_periodic::start;

struct RuntimeWorkflowCandidate {
    workflow_id: String,
    state: String,
    row_updated_at: chrono::DateTime<chrono::Utc>,
    repo: Option<String>,
    project_root: Option<PathBuf>,
    issue_number: Option<u64>,
    pr_number: u64,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReconciliationTransition {
    pub task_id: String,
    pub from: String,
    pub to: String,
    pub reason: String,
    pub applied: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowReconciliationTransition {
    pub workflow_id: String,
    pub from: String,
    pub to: String,
    pub reason: String,
    pub applied: bool,
    pub repo: Option<String>,
    pub issue_number: Option<u64>,
    pub pr_number: u64,
    pub pr_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowReconciliationAlert {
    pub workflow_id: String,
    pub state: String,
    pub reason: String,
    pub age_secs: u64,
    pub ttl_secs: u64,
    pub repo: Option<String>,
    pub issue_number: Option<u64>,
    pub pr_number: u64,
    pub pr_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReconciliationReport {
    pub candidates: usize,
    pub skipped_terminal: usize,
    #[serde(default)]
    pub transitions: Vec<ReconciliationTransition>,
    #[serde(default)]
    pub workflow_transitions: Vec<WorkflowReconciliationTransition>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub workflow_alerts: Vec<WorkflowReconciliationAlert>,
}

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

    async fn acquire(&mut self) {
        if self.max_per_minute == 0 {
            return;
        }
        if self.window_start.elapsed() >= Duration::from_secs(60) {
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

fn optional_json_string(data: &serde_json::Value, key: &str) -> Option<String> {
    data.get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

pub(crate) fn parse_external_id(eid: Option<&str>) -> (Option<u64>, Option<u64>) {
    match eid {
        Some(value) if value.starts_with("issue:") => (value["issue:".len()..].parse().ok(), None),
        Some(value) if value.starts_with("pr:") => (None, value["pr:".len()..].parse().ok()),
        _ => (None, None),
    }
}

pub async fn run_once_with_runtime_config(
    runtime_store: Option<&WorkflowRuntimeStore>,
    issue_workflows: Option<&IssueWorkflowStore>,
    config: &ReconciliationConfig,
    dry_run: bool,
    github_token: Option<&str>,
) -> ReconciliationReport {
    let Some(runtime_store) = runtime_store else {
        return ReconciliationReport {
            candidates: 0,
            skipped_terminal: 0,
            transitions: Vec::new(),
            workflow_transitions: Vec::new(),
            workflow_alerts: Vec::new(),
        };
    };
    let mut rate = RateLimiter::new(config.max_gh_calls_per_minute);
    match run_runtime_workflow_reconciliation_once(
        runtime_store,
        issue_workflows,
        &mut rate,
        RuntimeWorkflowReconciliationSettings::from_config(config),
        dry_run,
        github_token,
    )
    .await
    {
        Ok((candidates, skipped_terminal, workflow_transitions, workflow_alerts)) => {
            ReconciliationReport {
                candidates: candidates + skipped_terminal,
                skipped_terminal,
                transitions: Vec::new(),
                workflow_transitions,
                workflow_alerts,
            }
        }
        Err(error) => {
            tracing::warn!("workflow runtime reconciliation failed: {error}");
            ReconciliationReport {
                candidates: 0,
                skipped_terminal: 0,
                transitions: Vec::new(),
                workflow_transitions: Vec::new(),
                workflow_alerts: Vec::new(),
            }
        }
    }
}

fn runtime_transition_for_github_state(
    github_state: GitHubState,
) -> Option<(&'static str, &'static str)> {
    match github_state {
        GitHubState::PrMerged => Some(("done", "reconciled: PR merged externally")),
        GitHubState::PrClosed => Some(("cancelled", "reconciled: PR closed externally")),
        GitHubState::IssueClosed | GitHubState::Open | GitHubState::Unknown => None,
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
        let github_state = resolve_runtime_github_state(candidate, rate, github_token).await;
        if let Some(alert) = ready_to_merge_open_alert(candidate, github_state, settings, now) {
            alerts.push(alert);
            continue;
        }
        let Some((target_state, reason)) = runtime_transition_for_github_state(github_state) else {
            continue;
        };
        let applied = if dry_run {
            false
        } else {
            apply_runtime_workflow_transition(
                runtime_store,
                issue_workflows,
                candidate,
                target_state,
                reason,
            )
            .await
            .unwrap_or_else(|error| {
                tracing::warn!(
                    workflow_id = %candidate.workflow_id,
                    pr = candidate.pr_number,
                    repo = candidate.repo.as_deref(),
                    "workflow runtime reconciliation transition failed: {error}"
                );
                false
            })
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
    github_state: GitHubState,
    settings: RuntimeWorkflowReconciliationSettings,
    now: chrono::DateTime<chrono::Utc>,
) -> Option<WorkflowReconciliationAlert> {
    if candidate.state != "ready_to_merge" || github_state != GitHubState::Open {
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

async fn apply_runtime_workflow_transition(
    runtime_store: &WorkflowRuntimeStore,
    issue_workflows: Option<&IssueWorkflowStore>,
    candidate: &RuntimeWorkflowCandidate,
    target_state: &str,
    reason: &str,
) -> anyhow::Result<bool> {
    let Some(mut instance) = runtime_store.get_instance(&candidate.workflow_id).await? else {
        return Ok(false);
    };
    if instance.is_terminal() || instance.state != candidate.state {
        return Ok(false);
    }
    let event_type = if target_state == "done" {
        "PrMerged"
    } else {
        "PrClosed"
    };
    let command_type = if target_state == "done" {
        WorkflowCommandType::MarkDone
    } else {
        WorkflowCommandType::MarkCancelled
    };
    let decision_name = if target_state == "done" {
        "reconcile_pr_merged"
    } else {
        "reconcile_pr_closed"
    };
    let decision = WorkflowDecision::new(
        &instance.id,
        &instance.state,
        decision_name,
        target_state,
        reason,
    )
    .with_command(WorkflowCommand::new(
        command_type,
        format!(
            "runtime-reconcile:{}:{}:{}",
            instance.id, target_state, candidate.pr_number
        ),
        json!({
            "workflow_id": instance.id,
            "repo": candidate.repo.as_deref(),
            "issue_number": candidate.issue_number,
            "pr_number": candidate.pr_number,
            "pr_url": candidate.pr_url.as_deref(),
            "reason": reason,
        }),
    ))
    .with_evidence(WorkflowEvidence::new(
        "github_pr",
        runtime_pr_evidence_summary(candidate),
    ))
    .high_confidence();
    DecisionValidator::github_issue_pr().validate(
        &instance,
        &decision,
        &ValidationContext::new("reconciliation", chrono::Utc::now()),
    )?;

    instance.state = decision.next_state.clone();
    instance.version = instance.version.saturating_add(1);
    instance.data = merge_runtime_reconciliation_data(
        instance.data,
        decision_name,
        target_state,
        reason,
        candidate,
    );
    let event_payload = json!({
        "repo": candidate.repo.as_deref(),
        "issue_number": candidate.issue_number,
        "pr_number": candidate.pr_number,
        "pr_url": candidate.pr_url.as_deref(),
        "target_state": target_state,
        "reason": reason,
    });
    let Some(_) = runtime_store
        .apply_decision_transition(WorkflowDecisionTransition {
            expected_state: candidate.state.as_str(),
            create_if_missing: None,
            event_type,
            source: "reconciliation",
            payload: event_payload,
            decision: &decision,
            final_instance: &instance,
            command_status: WorkflowCommandStatus::Completed,
        })
        .await?
    else {
        return Ok(false);
    };
    record_runtime_issue_side_effects(issue_workflows, candidate, target_state, reason).await;
    Ok(true)
}

fn runtime_pr_evidence_summary(candidate: &RuntimeWorkflowCandidate) -> String {
    format!(
        "repo={} issue={} pr={} url={}",
        candidate.repo.as_deref().unwrap_or("<unknown>"),
        candidate
            .issue_number
            .map(|issue| issue.to_string())
            .unwrap_or_else(|| "<unknown>".to_string()),
        candidate.pr_number,
        candidate.pr_url.as_deref().unwrap_or("<unknown>")
    )
}

fn merge_runtime_reconciliation_data(
    mut data: serde_json::Value,
    decision: &str,
    target_state: &str,
    reason: &str,
    candidate: &RuntimeWorkflowCandidate,
) -> serde_json::Value {
    if let Some(object) = data.as_object_mut() {
        object.insert("last_decision".to_string(), json!(decision));
        object.insert("reconciled_at".to_string(), json!(chrono::Utc::now()));
        object.insert("reconciliation_reason".to_string(), json!(reason));
        object.insert("external_pr_state".to_string(), json!(target_state));
        object.insert("pr_number".to_string(), json!(candidate.pr_number));
        if let Some(pr_url) = candidate.pr_url.as_deref() {
            object.insert("pr_url".to_string(), json!(pr_url));
        }
        if let Some(repo) = candidate.repo.as_deref() {
            object.insert("repo".to_string(), json!(repo));
        }
        if let Some(issue_number) = candidate.issue_number {
            object.insert("issue_number".to_string(), json!(issue_number));
        }
    }
    data
}

async fn record_runtime_issue_side_effects(
    issue_workflows: Option<&IssueWorkflowStore>,
    candidate: &RuntimeWorkflowCandidate,
    target_state: &str,
    reason: &str,
) {
    let (Some(issue_workflows), Some(project_root)) =
        (issue_workflows, candidate.project_root.as_deref())
    else {
        return;
    };
    let project_id = project_root.to_string_lossy();
    let result = if target_state == "done" {
        if let Some(issue_number) = candidate.issue_number {
            issue_workflows
                .record_pr_merged_for_issue(
                    &project_id,
                    candidate.repo.as_deref(),
                    issue_number,
                    candidate.pr_number,
                    candidate.pr_url.as_deref(),
                    Some(reason),
                )
                .await
        } else {
            issue_workflows
                .record_pr_merged(
                    &project_id,
                    candidate.repo.as_deref(),
                    candidate.pr_number,
                    Some(reason),
                )
                .await
        }
    } else {
        issue_workflows
            .record_terminal_for_pr(
                &project_id,
                candidate.repo.as_deref(),
                candidate.pr_number,
                false,
                true,
                Some(reason),
            )
            .await
    };
    if let Err(error) = result {
        tracing::warn!(
            repo = candidate.repo.as_deref(),
            pr_number = candidate.pr_number,
            "reconciliation: issue workflow side effect failed: {error}"
        );
    }
}

#[cfg(test)]
#[path = "reconciliation_payload_tests.rs"]
mod payload_tests;
#[cfg(test)]
#[path = "reconciliation_tests.rs"]
mod tests;
