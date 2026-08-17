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
