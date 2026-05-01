use super::model::{WorkflowCommand, WorkflowDecision, WorkflowEvidence, WorkflowInstance};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IssueSubmissionWorkflowAction {
    RunImplementation,
}

#[derive(Debug, Clone, Copy)]
pub struct IssueSubmissionDecisionInput<'a> {
    pub task_id: &'a str,
    pub repo: Option<&'a str>,
    pub issue_number: u64,
    pub labels: &'a [String],
    pub force_execute: bool,
}

#[derive(Debug, Clone)]
pub struct IssueSubmissionDecisionOutput {
    pub action: IssueSubmissionWorkflowAction,
    pub decision: WorkflowDecision,
}

pub fn build_issue_submission_decision(
    instance: &WorkflowInstance,
    input: IssueSubmissionDecisionInput<'_>,
) -> IssueSubmissionDecisionOutput {
    let decision = WorkflowDecision::new(
        &instance.id,
        &instance.state,
        "submit_issue",
        "scheduled",
        "operator submitted the GitHub issue for implementation",
    )
    .with_command(WorkflowCommand::enqueue_activity(
        "implement_issue",
        format!(
            "issue-submit:{}:issue:{}:task:{}:implement",
            repo_key(input.repo),
            input.issue_number,
            input.task_id
        ),
    ))
    .with_evidence(WorkflowEvidence::new(
        "task_submission",
        format!(
            "task_id={} repo={} issue={} labels={} force_execute={}",
            input.task_id,
            repo_key(input.repo),
            input.issue_number,
            labels_summary(input.labels),
            input.force_execute
        ),
    ))
    .high_confidence();

    IssueSubmissionDecisionOutput {
        action: IssueSubmissionWorkflowAction::RunImplementation,
        decision,
    }
}

fn labels_summary(labels: &[String]) -> String {
    if labels.is_empty() {
        return "<none>".to_string();
    }
    labels.join(",")
}

fn repo_key(repo: Option<&str>) -> &str {
    repo.unwrap_or("<none>")
}
