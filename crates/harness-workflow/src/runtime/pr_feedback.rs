use super::model::{
    WorkflowCommand, WorkflowCommandType, WorkflowDecision, WorkflowEvidence, WorkflowInstance,
};
use super::quality_gate::{QUALITY_GATE_ACTIVITY, QUALITY_GATE_DEFINITION_ID};

pub const PR_FEEDBACK_DEFINITION_ID: &str = "pr_feedback";
pub const PR_FEEDBACK_INSPECT_ACTIVITY: &str = "inspect_pr_feedback";
pub const PR_FEEDBACK_SNAPSHOT_ARTIFACT: &str = "pr_feedback_snapshot";
pub const PR_REPAIR_SNAPSHOT_ARTIFACT: &str = "pr_repair_snapshot";
pub const SERVER_PR_SNAPSHOT_ARTIFACT: &str = "server_pr_snapshot";
pub const LOCAL_REVIEW_ACTIVITY: &str = "run_local_review";
pub const LOCAL_REVIEW_PASSED_SIGNAL: &str = "LocalReviewPassed";
pub const LOCAL_REVIEW_CHANGES_REQUESTED_SIGNAL: &str = "LocalReviewChangesRequested";
pub const LOCAL_REVIEW_BLOCKED_SIGNAL: &str = "LocalReviewBlocked";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrFeedbackWorkflowAction {
    BindPr,
    RequestLocalReview,
    LocalReviewPassed,
    LocalReviewChangesRequested,
    LocalReviewBlocked,
    SweepFeedback,
    InspectFeedback,
    AddressFeedback,
    AwaitFeedback,
    RequestQualityGate,
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
pub struct LocalReviewDecisionInput<'a> {
    pub dedupe_key: &'a str,
    pub pr_number: u64,
    pub pr_url: Option<&'a str>,
    pub issue_number: Option<u64>,
    pub repo: Option<&'a str>,
    pub summary: &'a str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalReviewOutcome {
    Passed,
    ChangesRequested,
    Blocked,
}

#[derive(Debug, Clone, Copy)]
pub struct LocalReviewCompletedInput<'a> {
    pub task_id: &'a str,
    pub pr_number: u64,
    pub pr_url: Option<&'a str>,
    pub repair_dedupe_key: &'a str,
    pub outcome: LocalReviewOutcome,
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
                "start_quality_gate",
                "quality_gate_pending",
                input.summary,
            )
            .with_command(WorkflowCommand::new(
                super::model::WorkflowCommandType::StartChildWorkflow,
                format!("quality-gate:{}:{}:run", input.task_id, input.pr_number),
                serde_json::json!({
                    "definition_id": QUALITY_GATE_DEFINITION_ID,
                    "subject_key": format!("pr:{}", input.pr_number),
                    "child_activity": QUALITY_GATE_ACTIVITY,
                    "pr_number": input.pr_number,
                    "pr_url": input.pr_url,
                    "validation_commands": [],
                }),
            ))
            .with_evidence(feedback_evidence(input))
            .high_confidence();
            PrFeedbackDecisionOutput {
                action: PrFeedbackWorkflowAction::RequestQualityGate,
                decision,
            }
        }
    }
}

pub fn build_local_review_request_decision(
    instance: &WorkflowInstance,
    input: LocalReviewDecisionInput<'_>,
) -> PrFeedbackDecisionOutput {
    let decision = WorkflowDecision::new(
        &instance.id,
        &instance.state,
        "run_local_review",
        "local_review_gate",
        input.summary,
    )
    .with_command(WorkflowCommand::new(
        WorkflowCommandType::EnqueueActivity,
        input.dedupe_key,
        serde_json::json!({
            "activity": LOCAL_REVIEW_ACTIVITY,
            "pr_number": input.pr_number,
            "pr_url": input.pr_url,
            "issue_number": input.issue_number,
            "repo": input.repo,
        }),
    ))
    .with_evidence(WorkflowEvidence::new(
        "local_review_request",
        local_review_summary(input),
    ))
    .high_confidence();

    PrFeedbackDecisionOutput {
        action: PrFeedbackWorkflowAction::RequestLocalReview,
        decision,
    }
}

pub fn build_local_review_completed_decision(
    instance: &WorkflowInstance,
    input: LocalReviewCompletedInput<'_>,
) -> PrFeedbackDecisionOutput {
    match input.outcome {
        LocalReviewOutcome::Passed => {
            let decision = WorkflowDecision::new(
                &instance.id,
                &instance.state,
                "local_review_passed",
                "awaiting_feedback",
                input.summary,
            )
            .with_command(WorkflowCommand::wait(
                "Local review passed; waiting for remote review, checks, and mergeability.",
                format!("local-review:{}:{}:passed", input.task_id, input.pr_number),
            ))
            .with_evidence(local_review_evidence(input))
            .high_confidence();
            PrFeedbackDecisionOutput {
                action: PrFeedbackWorkflowAction::LocalReviewPassed,
                decision,
            }
        }
        LocalReviewOutcome::ChangesRequested => {
            let decision = WorkflowDecision::new(
                &instance.id,
                &instance.state,
                "address_local_review_feedback",
                "addressing_feedback",
                input.summary,
            )
            .with_command(WorkflowCommand::new(
                super::model::WorkflowCommandType::EnqueueActivity,
                input.repair_dedupe_key,
                serde_json::json!({
                    "activity": "address_pr_feedback",
                    "source": "local_review",
                    "task_id": input.task_id,
                    "pr_number": input.pr_number,
                    "pr_url": input.pr_url,
                    "review_summary": input.summary,
                }),
            ))
            .with_evidence(local_review_evidence(input));
            PrFeedbackDecisionOutput {
                action: PrFeedbackWorkflowAction::LocalReviewChangesRequested,
                decision,
            }
        }
        LocalReviewOutcome::Blocked => {
            let decision = WorkflowDecision::new(
                &instance.id,
                &instance.state,
                "local_review_blocked",
                "blocked",
                input.summary,
            )
            .with_command(WorkflowCommand::mark_blocked(
                input.summary,
                format!("local-review:{}:{}:blocked", input.task_id, input.pr_number),
            ))
            .with_evidence(local_review_evidence(input));
            PrFeedbackDecisionOutput {
                action: PrFeedbackWorkflowAction::LocalReviewBlocked,
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

fn local_review_evidence(input: LocalReviewCompletedInput<'_>) -> WorkflowEvidence {
    let summary = match input.pr_url {
        Some(pr_url) => format!("pr={} url={} {}", input.pr_number, pr_url, input.summary),
        None => format!("pr={} {}", input.pr_number, input.summary),
    };
    WorkflowEvidence::new("local_review", summary)
}

fn local_review_summary(input: LocalReviewDecisionInput<'_>) -> String {
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
