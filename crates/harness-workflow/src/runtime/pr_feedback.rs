use super::model::{WorkflowCommand, WorkflowDecision, WorkflowEvidence, WorkflowInstance};

pub const PR_FEEDBACK_DEFINITION_ID: &str = "pr_feedback";
pub const PR_FEEDBACK_INSPECT_ACTIVITY: &str = "inspect_pr_feedback";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrFeedbackWorkflowAction {
    BindPr,
    SweepFeedback,
    InspectFeedback,
    AddressFeedback,
    AwaitFeedback,
    ReadyToMerge,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrFeedbackOutcome {
    BlockingFeedback,
    NoActionableFeedback,
    ReadyToMerge,
}

#[derive(Debug, Clone, Copy)]
pub struct PrDetectedDecisionInput<'a> {
    pub task_id: &'a str,
    pub pr_number: u64,
    pub pr_url: &'a str,
}

#[derive(Debug, Clone, Copy)]
pub struct PrFeedbackDecisionInput<'a> {
    pub task_id: &'a str,
    pub pr_number: u64,
    pub pr_url: Option<&'a str>,
    pub outcome: PrFeedbackOutcome,
    pub summary: &'a str,
}

#[derive(Debug, Clone)]
pub struct PrFeedbackSweepDecisionInput<'a> {
    pub dedupe_key: &'a str,
    pub pr_number: u64,
    pub pr_url: Option<&'a str>,
    pub issue_number: Option<u64>,
    pub repo: Option<&'a str>,
    pub summary: &'a str,
}

#[derive(Debug, Clone)]
pub struct PrFeedbackInspectDecisionInput<'a> {
    pub dedupe_key: &'a str,
    pub pr_number: u64,
    pub pr_url: Option<&'a str>,
    pub issue_number: Option<u64>,
    pub repo: Option<&'a str>,
    pub parent_workflow_id: Option<&'a str>,
    pub summary: &'a str,
}

#[derive(Debug, Clone)]
pub struct PrFeedbackDecisionOutput {
    pub action: PrFeedbackWorkflowAction,
    pub decision: WorkflowDecision,
}

pub fn build_pr_detected_decision(
    instance: &WorkflowInstance,
    input: PrDetectedDecisionInput<'_>,
) -> PrFeedbackDecisionOutput {
    let decision = WorkflowDecision::new(
        &instance.id,
        &instance.state,
        "bind_pr",
        "pr_open",
        "implementation produced a pull request for the issue workflow",
    )
    .with_command(WorkflowCommand::bind_pr(
        input.pr_number,
        input.pr_url,
        format!("pr-detected:{}:{}", input.task_id, input.pr_number),
    ))
    .with_evidence(WorkflowEvidence::new("pr", input.pr_url))
    .high_confidence();

    PrFeedbackDecisionOutput {
        action: PrFeedbackWorkflowAction::BindPr,
        decision,
    }
}

pub fn build_pr_feedback_decision(
    instance: &WorkflowInstance,
    input: PrFeedbackDecisionInput<'_>,
) -> PrFeedbackDecisionOutput {
    match input.outcome {
        PrFeedbackOutcome::BlockingFeedback => {
            let decision = WorkflowDecision::new(
                &instance.id,
                &instance.state,
                "address_pr_feedback",
                "addressing_feedback",
                input.summary,
            )
            .with_command(WorkflowCommand::enqueue_activity(
                "address_pr_feedback",
                format!("pr-feedback:{}:{}:address", input.task_id, input.pr_number),
            ))
            .with_evidence(feedback_evidence(input));
            PrFeedbackDecisionOutput {
                action: PrFeedbackWorkflowAction::AddressFeedback,
                decision,
            }
        }
        PrFeedbackOutcome::NoActionableFeedback => {
            let decision = WorkflowDecision::new(
                &instance.id,
                &instance.state,
                "wait_for_pr_feedback",
                "awaiting_feedback",
                input.summary,
            )
            .with_command(WorkflowCommand::wait(
                input.summary,
                format!("pr-feedback:{}:{}:wait", input.task_id, input.pr_number),
            ))
            .with_evidence(feedback_evidence(input));
            PrFeedbackDecisionOutput {
                action: PrFeedbackWorkflowAction::AwaitFeedback,
                decision,
            }
        }
        PrFeedbackOutcome::ReadyToMerge => {
            let decision = WorkflowDecision::new(
                &instance.id,
                &instance.state,
                "mark_ready_to_merge",
                "ready_to_merge",
                input.summary,
            )
            .with_evidence(feedback_evidence(input))
            .high_confidence();
            PrFeedbackDecisionOutput {
                action: PrFeedbackWorkflowAction::ReadyToMerge,
                decision,
            }
        }
    }
}

pub fn build_pr_feedback_sweep_decision(
    instance: &WorkflowInstance,
    input: PrFeedbackSweepDecisionInput<'_>,
) -> PrFeedbackDecisionOutput {
    let decision = WorkflowDecision::new(
        &instance.id,
        &instance.state,
        "sweep_pr_feedback",
        "awaiting_feedback",
        input.summary,
    )
    .with_command(WorkflowCommand::new(
        super::model::WorkflowCommandType::StartChildWorkflow,
        input.dedupe_key,
        serde_json::json!({
            "definition_id": PR_FEEDBACK_DEFINITION_ID,
            "subject_key": format!("pr:{}", input.pr_number),
            "child_activity": PR_FEEDBACK_INSPECT_ACTIVITY,
            "pr_number": input.pr_number,
            "pr_url": input.pr_url,
            "issue_number": input.issue_number,
            "repo": input.repo,
        }),
    ))
    .with_evidence(WorkflowEvidence::new(
        "pr_feedback_sweep",
        feedback_sweep_summary(input),
    ))
    .high_confidence();

    PrFeedbackDecisionOutput {
        action: PrFeedbackWorkflowAction::SweepFeedback,
        decision,
    }
}

pub fn build_pr_feedback_inspect_decision(
    instance: &WorkflowInstance,
    input: PrFeedbackInspectDecisionInput<'_>,
) -> PrFeedbackDecisionOutput {
    let decision = WorkflowDecision::new(
        &instance.id,
        &instance.state,
        "inspect_pr_feedback",
        "inspecting",
        input.summary,
    )
    .with_command(WorkflowCommand::new(
        super::model::WorkflowCommandType::EnqueueActivity,
        input.dedupe_key,
        serde_json::json!({
            "activity": PR_FEEDBACK_INSPECT_ACTIVITY,
            "pr_number": input.pr_number,
            "pr_url": input.pr_url,
            "issue_number": input.issue_number,
            "repo": input.repo,
            "parent_workflow_id": input.parent_workflow_id,
        }),
    ))
    .with_evidence(WorkflowEvidence::new(
        "pr_feedback_child",
        feedback_inspect_summary(input),
    ))
    .high_confidence();

    PrFeedbackDecisionOutput {
        action: PrFeedbackWorkflowAction::InspectFeedback,
        decision,
    }
}

fn feedback_evidence(input: PrFeedbackDecisionInput<'_>) -> WorkflowEvidence {
    let summary = match input.pr_url {
        Some(pr_url) => format!("pr={} url={} {}", input.pr_number, pr_url, input.summary),
        None => format!("pr={} {}", input.pr_number, input.summary),
    };
    WorkflowEvidence::new("pr_feedback", summary)
}

fn feedback_inspect_summary(input: PrFeedbackInspectDecisionInput<'_>) -> String {
    let mut parts = vec![format!("pr={}", input.pr_number)];
    if let Some(pr_url) = input.pr_url {
        parts.push(format!("url={pr_url}"));
    }
    if let Some(issue_number) = input.issue_number {
        parts.push(format!("issue={issue_number}"));
    }
    if let Some(repo) = input.repo {
        parts.push(format!("repo={repo}"));
    }
    if let Some(parent_workflow_id) = input.parent_workflow_id {
        parts.push(format!("parent={parent_workflow_id}"));
    }
    parts.push(input.summary.to_string());
    parts.join(" ")
}

fn feedback_sweep_summary(input: PrFeedbackSweepDecisionInput<'_>) -> String {
    let mut parts = vec![format!("pr={}", input.pr_number)];
    if let Some(pr_url) = input.pr_url {
        parts.push(format!("url={pr_url}"));
    }
    if let Some(issue_number) = input.issue_number {
        parts.push(format!("issue={issue_number}"));
    }
    if let Some(repo) = input.repo {
        parts.push(format!("repo={repo}"));
    }
    parts.push(input.summary.to_string());
    parts.join(" ")
}
