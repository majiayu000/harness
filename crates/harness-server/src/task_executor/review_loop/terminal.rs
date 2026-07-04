use crate::task_runner::{
    mark_terminal_once, TaskId, TaskStatus, TaskStore, TaskTerminalFailure, TaskTerminalOutcome,
};

pub(super) async fn mark_round_budget_exhausted(
    store: &TaskStore,
    task_id: &TaskId,
    rounds_used: u32,
    last_status: TaskStatus,
) -> anyhow::Result<()> {
    let outcome = TaskTerminalOutcome::Failed(TaskTerminalFailure::round_budget_exhausted(
        rounds_used,
        last_status.clone(),
        Some("local_review_gate".to_string()),
    ));
    let transition = mark_terminal_once(store, task_id, outcome).await?;
    if transition.applied() {
        tracing::error!(
            task_id = %task_id,
            rounds_used,
            last_status = last_status.as_ref(),
            "task marked terminal after review round budget exhaustion"
        );
    } else {
        tracing::debug!(
            task_id = %task_id,
            ?transition,
            "round budget exhaustion skipped because task was already terminal or missing"
        );
    }
    Ok(())
}
