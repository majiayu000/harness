use super::{WorkflowCommand, WorkflowCommandType};

pub(super) fn is_replan_command(command: &WorkflowCommand) -> bool {
    command.activity_name() == Some("replan_issue")
}

pub(super) fn required_command_for_transition(
    from_state: &str,
    to_state: &str,
) -> Option<WorkflowCommandType> {
    match (from_state, to_state) {
        (from_state, "pr_open") if from_state != "pr_open" => Some(WorkflowCommandType::BindPr),
        ("idle", "scanning") => Some(WorkflowCommandType::EnqueueActivity),
        ("scanning", "planning_batch") => Some(WorkflowCommandType::EnqueueActivity),
        ("planning_batch", "dispatching") => Some(WorkflowCommandType::StartChildWorkflow),
        (_, "done") => Some(WorkflowCommandType::MarkDone),
        (_, "blocked") => Some(WorkflowCommandType::MarkBlocked),
        (_, "failed") => Some(WorkflowCommandType::MarkFailed),
        (_, "cancelled") => Some(WorkflowCommandType::MarkCancelled),
        _ => None,
    }
}

use super::{
    DecisionValidator, ValidationContext, WorkflowDecisionRejection, WorkflowDecisionRejectionKind,
};
use std::collections::BTreeSet;

impl DecisionValidator {
    pub(super) fn validate_dedupe(
        &self,
        command: &WorkflowCommand,
        seen_dedupe_keys: &mut BTreeSet<String>,
        context: &ValidationContext,
    ) -> Result<(), WorkflowDecisionRejection> {
        if command.dedupe_key.trim().is_empty() {
            return Err(WorkflowDecisionRejection::new(
                WorkflowDecisionRejectionKind::MissingDedupeKey,
                "workflow commands must include a non-empty dedupe key",
            ));
        }

        if !seen_dedupe_keys.insert(command.dedupe_key.clone()) {
            return Err(WorkflowDecisionRejection::new(
                WorkflowDecisionRejectionKind::DuplicateCommandDedupeKey,
                format!(
                    "decision contains duplicate command dedupe key '{}'",
                    command.dedupe_key
                ),
            ));
        }

        if context.active_dedupe_keys.contains(&command.dedupe_key) {
            return Err(WorkflowDecisionRejection::new(
                WorkflowDecisionRejectionKind::ActiveDuplicateCommand,
                format!(
                    "an active command already owns dedupe key '{}'",
                    command.dedupe_key
                ),
            ));
        }

        Ok(())
    }
}
