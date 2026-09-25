use super::{
    github_issue_pr_validation, validator_progress, DecisionValidator, DecisionValidatorKind,
    TransitionRule, ValidationContext, WorkflowDecisionRejection, WorkflowDecisionRejectionKind,
};
use crate::runtime::model::{WorkflowCommandType, WorkflowDecision, WorkflowInstance};
use harness_core::claim_trust::ClaimTrustLevel;

impl DecisionValidator {
    pub(super) fn validate_hidden_workflow_transition(
        &self,
        instance: &WorkflowInstance,
        decision: &WorkflowDecision,
        context: &ValidationContext,
    ) -> Result<bool, WorkflowDecisionRejection> {
        if self.kind == DecisionValidatorKind::GithubIssuePr
            && decision.decision == "recover_github_pr_coverage"
        {
            self.validate_github_coverage_recovery_transition(instance, decision, context)?;
            return Ok(true);
        }

        if self.kind != DecisionValidatorKind::GithubIssuePr
            || !github_issue_pr_validation::is_reconciliation_only_done_transition(decision)
        {
            return Ok(false);
        }

        let rule = TransitionRule::new(
            decision.observed_state.as_str(),
            "done",
            [WorkflowCommandType::MarkDone],
        );
        self.validate_commands(&rule, decision, context)?;
        validator_progress::validate_target_progress_contract(
            self.state_definition(&decision.next_state),
            instance,
            decision,
        )?;
        github_issue_pr_validation::validate_reconciliation_only_done(decision, context)?;
        Ok(true)
    }

    fn validate_github_coverage_recovery_transition(
        &self,
        instance: &WorkflowInstance,
        decision: &WorkflowDecision,
        context: &ValidationContext,
    ) -> Result<(), WorkflowDecisionRejection> {
        if context.actor != "reconciliation" {
            return Err(WorkflowDecisionRejection::new(
                WorkflowDecisionRejectionKind::TransitionNotAllowed,
                "GitHub coverage recovery transitions require reconciliation context",
            ));
        }
        let mut evidence_rule = TransitionRule::new(
            decision.observed_state.as_str(),
            decision.next_state.as_str(),
            std::iter::empty::<WorkflowCommandType>(),
        );
        evidence_rule
            .required_evidence
            .insert("server_pr_snapshot".to_string());
        evidence_rule.required_evidence_trust.insert(
            "server_pr_snapshot".to_string(),
            ClaimTrustLevel::RuntimeObserved,
        );
        validator_progress::validate_required_evidence(&evidence_rule, decision)?;

        let required_command = coverage_recovery_required_command(decision)?;
        if !decision
            .commands
            .iter()
            .any(|command| command.command_type == required_command)
        {
            return Err(WorkflowDecisionRejection::new(
                WorkflowDecisionRejectionKind::RequiredCommandMissing,
                format!(
                    "GitHub coverage recovery transition '{}' -> '{}' requires command {:?}",
                    decision.observed_state, decision.next_state, required_command
                ),
            ));
        }

        let rule = TransitionRule::new(
            decision.observed_state.as_str(),
            decision.next_state.as_str(),
            coverage_recovery_allowed_commands(decision.next_state.as_str()),
        );
        self.validate_commands(&rule, decision, context)?;
        validator_progress::validate_target_progress_contract(
            self.state_definition(&decision.next_state),
            instance,
            decision,
        )
    }
}

fn coverage_recovery_required_command(
    decision: &WorkflowDecision,
) -> Result<WorkflowCommandType, WorkflowDecisionRejection> {
    match decision.next_state.as_str() {
        "pr_open" | "awaiting_feedback" => Ok(WorkflowCommandType::BindPr),
        "quality_gate_pending" => Ok(WorkflowCommandType::StartChildWorkflow),
        "done" => Ok(WorkflowCommandType::MarkDone),
        "cancelled" => Ok(WorkflowCommandType::MarkCancelled),
        _ => Err(WorkflowDecisionRejection::new(
            WorkflowDecisionRejectionKind::TransitionNotAllowed,
            format!(
                "GitHub coverage recovery cannot transition to '{}'",
                decision.next_state
            ),
        )),
    }
}

fn coverage_recovery_allowed_commands(next_state: &str) -> Vec<WorkflowCommandType> {
    match next_state {
        "pr_open" | "awaiting_feedback" => vec![WorkflowCommandType::BindPr],
        "quality_gate_pending" => vec![
            WorkflowCommandType::BindPr,
            WorkflowCommandType::StartChildWorkflow,
        ],
        "done" => vec![WorkflowCommandType::BindPr, WorkflowCommandType::MarkDone],
        "cancelled" => vec![
            WorkflowCommandType::BindPr,
            WorkflowCommandType::MarkCancelled,
        ],
        _ => Vec::new(),
    }
}
