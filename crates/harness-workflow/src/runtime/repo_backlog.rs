use super::model::{
    WorkflowCommand, WorkflowCommandType, WorkflowDecision, WorkflowEvidence, WorkflowInstance,
};

pub const REPO_BACKLOG_DEFINITION_ID: &str = "repo_backlog";
pub const REPO_BACKLOG_POLL_ACTIVITY: &str = "poll_repo_backlog";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepoBacklogWorkflowAction {
    PollBacklog,
    StartIssueWorkflow,
    MarkBoundIssueDone,
    RequestRecovery,
}

#[derive(Debug, Clone, Copy)]
pub struct RepoBacklogPollDecisionInput<'a> {
    pub repo: Option<&'a str>,
    pub label: Option<&'a str>,
    pub dedupe_key: &'a str,
}

#[derive(Debug, Clone, Copy)]
pub struct OpenIssueDecisionInput<'a> {
    pub repo: Option<&'a str>,
    pub issue_number: u64,
    pub issue_url: Option<&'a str>,
}

#[derive(Debug, Clone, Copy)]
pub struct MergedPrDecisionInput<'a> {
    pub repo: Option<&'a str>,
    pub issue_number: Option<u64>,
    pub pr_number: u64,
    pub pr_url: Option<&'a str>,
}

#[derive(Debug, Clone, Copy)]
pub struct StaleWorkflowDecisionInput<'a> {
    pub repo: Option<&'a str>,
    pub issue_number: u64,
    pub active_task_id: Option<&'a str>,
    pub observed_state: &'a str,
    pub reason: &'a str,
}

#[derive(Debug, Clone)]
pub struct RepoBacklogDecisionOutput {
    pub action: RepoBacklogWorkflowAction,
    pub decision: WorkflowDecision,
}

pub fn repo_backlog_workflow_id(project_id: &str, repo: Option<&str>) -> String {
    format!("{project_id}::repo:{}::backlog", repo.unwrap_or("<none>"))
}

pub fn build_repo_backlog_poll_decision(
    instance: &WorkflowInstance,
    input: RepoBacklogPollDecisionInput<'_>,
) -> RepoBacklogDecisionOutput {
    let decision = WorkflowDecision::new(
        &instance.id,
        &instance.state,
        "poll_repo_backlog",
        "scanning",
        "repo backlog polling should be executed by a runtime agent",
    )
    .with_command(WorkflowCommand::new(
        WorkflowCommandType::EnqueueActivity,
        input.dedupe_key,
        serde_json::json!({
            "activity": REPO_BACKLOG_POLL_ACTIVITY,
            "repo": input.repo,
            "label": input.label,
        }),
    ))
    .with_evidence(WorkflowEvidence::new(
        "repo_backlog_poll",
        format!(
            "repo={} label={}",
            repo_key(input.repo),
            input.label.unwrap_or("<none>")
        ),
    ))
    .high_confidence();

    RepoBacklogDecisionOutput {
        action: RepoBacklogWorkflowAction::PollBacklog,
        decision,
    }
}

pub fn build_open_issue_without_workflow_decision(
    instance: &WorkflowInstance,
    input: OpenIssueDecisionInput<'_>,
) -> RepoBacklogDecisionOutput {
    let decision = WorkflowDecision::new(
        &instance.id,
        &instance.state,
        "start_issue_workflow",
        "dispatching",
        "open issue has no durable issue workflow yet",
    )
    .with_command(WorkflowCommand::start_child_workflow(
        "github_issue_pr",
        format!("issue:{}", input.issue_number),
        format!(
            "repo-backlog:{}:issue:{}:start",
            repo_key(input.repo),
            input.issue_number
        ),
    ))
    .with_evidence(issue_evidence(input))
    .high_confidence();

    RepoBacklogDecisionOutput {
        action: RepoBacklogWorkflowAction::StartIssueWorkflow,
        decision,
    }
}

pub fn build_merged_pr_decision(
    instance: &WorkflowInstance,
    input: MergedPrDecisionInput<'_>,
) -> RepoBacklogDecisionOutput {
    let decision = WorkflowDecision::new(
        &instance.id,
        &instance.state,
        "mark_bound_issue_done",
        "reconciling",
        "merged PR should close the bound issue workflow",
    )
    .with_command(WorkflowCommand::enqueue_activity(
        "mark_bound_issue_done",
        format!(
            "repo-backlog:{}:pr:{}:merged",
            repo_key(input.repo),
            input.pr_number
        ),
    ))
    .with_evidence(pr_evidence(input))
    .high_confidence();

    RepoBacklogDecisionOutput {
        action: RepoBacklogWorkflowAction::MarkBoundIssueDone,
        decision,
    }
}

pub fn build_stale_active_workflow_decision(
    instance: &WorkflowInstance,
    input: StaleWorkflowDecisionInput<'_>,
) -> RepoBacklogDecisionOutput {
    let decision = WorkflowDecision::new(
        &instance.id,
        &instance.state,
        "request_workflow_recovery",
        "reconciling",
        input.reason,
    )
    .with_command(WorkflowCommand::enqueue_activity(
        "recover_issue_workflow",
        format!(
            "repo-backlog:{}:issue:{}:recover",
            repo_key(input.repo),
            input.issue_number
        ),
    ))
    .with_evidence(WorkflowEvidence::new(
        "stale_issue_workflow",
        format!(
            "issue={} state={} active_task_id={} reason={}",
            input.issue_number,
            input.observed_state,
            input.active_task_id.unwrap_or("<none>"),
            input.reason
        ),
    ));

    RepoBacklogDecisionOutput {
        action: RepoBacklogWorkflowAction::RequestRecovery,
        decision,
    }
}

fn issue_evidence(input: OpenIssueDecisionInput<'_>) -> WorkflowEvidence {
    let summary = match input.issue_url {
        Some(url) => format!(
            "repo={} issue={} url={}",
            repo_key(input.repo),
            input.issue_number,
            url
        ),
        None => format!("repo={} issue={}", repo_key(input.repo), input.issue_number),
    };
    WorkflowEvidence::new("github_issue", summary)
}

fn pr_evidence(input: MergedPrDecisionInput<'_>) -> WorkflowEvidence {
    let issue = input
        .issue_number
        .map(|issue_number| issue_number.to_string())
        .unwrap_or_else(|| "<unknown>".to_string());
    let url = input.pr_url.unwrap_or("<unknown>");
    WorkflowEvidence::new(
        "github_pr",
        format!(
            "repo={} issue={} pr={} url={}",
            repo_key(input.repo),
            issue,
            input.pr_number,
            url
        ),
    )
}

fn repo_key(repo: Option<&str>) -> &str {
    repo.unwrap_or("<none>")
}
