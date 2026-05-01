use super::model::{WorkflowCommand, WorkflowDecision, WorkflowEvidence, WorkflowInstance};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanIssueWorkflowAction {
    RunReplan,
    ForceContinue,
    Block,
}

#[derive(Debug, Clone, Copy)]
pub struct PlanIssueDecisionInput<'a> {
    pub task_id: &'a str,
    pub plan_issue: &'a str,
    pub force_execute: bool,
    pub auto_replan_on_plan_issue: bool,
    pub replan_already_attempted: bool,
    pub turn_budget_exhausted: bool,
}

#[derive(Debug, Clone)]
pub struct PlanIssueDecisionOutput {
    pub action: PlanIssueWorkflowAction,
    pub decision: WorkflowDecision,
}

pub fn build_plan_issue_decision(
    instance: &WorkflowInstance,
    input: PlanIssueDecisionInput<'_>,
) -> PlanIssueDecisionOutput {
    if input.replan_already_attempted {
        return block_decision(
            instance,
            input.task_id,
            input.plan_issue,
            format!("PLAN_ISSUE persisted after replan: {}", input.plan_issue),
            "block_replan_loop",
        );
    }

    if input.force_execute {
        let decision = WorkflowDecision::new(
            &instance.id,
            &instance.state,
            "continue_force_execute",
            "implementing",
            "force_execute policy accepts the plan concern and continues implementation",
        )
        .with_command(WorkflowCommand::record_plan_concern(
            input.plan_issue,
            format!("plan-issue:{}:concern", input.task_id),
        ))
        .with_command(WorkflowCommand::enqueue_activity(
            "implement_issue",
            format!("plan-issue:{}:force-continue", input.task_id),
        ))
        .with_evidence(WorkflowEvidence::new("plan_issue", input.plan_issue))
        .high_confidence();
        return PlanIssueDecisionOutput {
            action: PlanIssueWorkflowAction::ForceContinue,
            decision,
        };
    }

    if !input.auto_replan_on_plan_issue {
        return block_decision(
            instance,
            input.task_id,
            input.plan_issue,
            format!(
                "PLAN_ISSUE encountered and auto_replan_on_plan_issue=false: {}",
                input.plan_issue
            ),
            "block_replan_disabled",
        );
    }

    if input.turn_budget_exhausted {
        return block_decision(
            instance,
            input.task_id,
            input.plan_issue,
            "Turn budget exhausted before replan".to_string(),
            "block_replan_budget",
        );
    }

    let decision = WorkflowDecision::new(
        &instance.id,
        &instance.state,
        "run_replan",
        "replanning",
        "implementation raised a plan concern and replan policy is enabled",
    )
    .with_command(WorkflowCommand::record_plan_concern(
        input.plan_issue,
        format!("plan-issue:{}:concern", input.task_id),
    ))
    .with_command(WorkflowCommand::enqueue_activity(
        "replan_issue",
        format!("plan-issue:{}:replan", input.task_id),
    ))
    .with_evidence(WorkflowEvidence::new("plan_issue", input.plan_issue))
    .high_confidence();

    PlanIssueDecisionOutput {
        action: PlanIssueWorkflowAction::RunReplan,
        decision,
    }
}

fn block_decision(
    instance: &WorkflowInstance,
    task_id: &str,
    plan_issue: &str,
    reason: String,
    decision_name: &str,
) -> PlanIssueDecisionOutput {
    let decision = WorkflowDecision::new(
        &instance.id,
        &instance.state,
        decision_name,
        "blocked",
        &reason,
    )
    .with_command(WorkflowCommand::mark_blocked(
        &reason,
        format!("plan-issue:{task_id}:blocked"),
    ))
    .with_evidence(WorkflowEvidence::new("plan_issue", plan_issue))
    .high_confidence();
    PlanIssueDecisionOutput {
        action: PlanIssueWorkflowAction::Block,
        decision,
    }
}
