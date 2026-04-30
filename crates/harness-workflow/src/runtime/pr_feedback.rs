use super::model::{WorkflowCommand, WorkflowDecision, WorkflowEvidence, WorkflowInstance};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrFeedbackWorkflowAction {
    BindPr,
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

fn feedback_evidence(input: PrFeedbackDecisionInput<'_>) -> WorkflowEvidence {
    let summary = match input.pr_url {
        Some(pr_url) => format!("pr={} url={} {}", input.pr_number, pr_url, input.summary),
        None => format!("pr={} {}", input.pr_number, input.summary),
    };
    WorkflowEvidence::new("pr_feedback", summary)
}
