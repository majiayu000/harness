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
    pub additional_prompt: Option<&'a str>,
    pub depends_on: &'a [String],
    pub dependencies_blocked: bool,
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
    let next_state = if input.dependencies_blocked {
        "awaiting_dependencies"
    } else {
        "implementing"
    };
    let reason = if input.dependencies_blocked {
        "operator submitted the GitHub issue and it is waiting for dependencies"
    } else {
        "operator submitted the GitHub issue for implementation"
    };
    let mut decision = WorkflowDecision::new(
        &instance.id,
        &instance.state,
        "submit_issue",
        next_state,
        reason,
    );
    if !input.dependencies_blocked {
        decision = decision.with_command(WorkflowCommand::new(
            super::model::WorkflowCommandType::EnqueueActivity,
            format!(
                "issue-submit:{}:issue:{}:task:{}:implement",
                repo_key(input.repo),
                input.issue_number,
                input.task_id
            ),
            serde_json::json!({
                "activity": "implement_issue",
                "additional_prompt": input.additional_prompt,
            }),
        ));
    }
    let decision = decision.with_evidence(WorkflowEvidence::new(
        "task_submission",
        format!(
            "task_id={} repo={} issue={} labels={} force_execute={} additional_prompt={} depends_on={} dependencies_blocked={}",
            input.task_id,
            repo_key(input.repo),
            input.issue_number,
            labels_summary(input.labels),
            input.force_execute,
            input.additional_prompt.is_some(),
            depends_on_summary(input.depends_on),
            input.dependencies_blocked
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

fn depends_on_summary(depends_on: &[String]) -> String {
    if depends_on.is_empty() {
        return "<none>".to_string();
    }
    depends_on.join(",")
}

fn repo_key(repo: Option<&str>) -> &str {
    repo.unwrap_or("<none>")
}
