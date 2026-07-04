use super::super::state::{RoundResult, SchedulerAuthorityState, TaskState};
use super::super::store::{mutate_and_persist, TaskStore};
use super::super::types::{TaskFailureKind, TaskId, TaskStatus};
use super::workspace_lifecycle::log_task_failure_event;

fn spawn_exit_requeued(state: &TaskState) -> bool {
    matches!(
        (&state.status, &state.scheduler.authority_state),
        (TaskStatus::Pending, SchedulerAuthorityState::Queued)
            | (TaskStatus::Pending, SchedulerAuthorityState::RetryBackoff)
            | (
                TaskStatus::AwaitingDeps,
                SchedulerAuthorityState::AwaitingDependencies
            )
    )
}

pub(crate) fn spawn_exit_state_is_allowed(state: &TaskState) -> bool {
    state.status.is_terminal() || spawn_exit_requeued(state)
}

fn spawn_exit_invariant_reason(context: &str, state: &TaskState) -> String {
    format!(
        "spawn exit invariant violated: {context} exited with non-terminal status '{}' and scheduler state '{}'",
        state.status.as_ref(),
        state.scheduler.authority_state.as_ref()
    )
}

pub(super) async fn ensure_terminal_or_requeued_on_spawn_exit(
    store: &TaskStore,
    events: &harness_observe::event_store::EventStore,
    task_id: &TaskId,
    context: &str,
) {
    let Some(snapshot) = store.get(task_id) else {
        tracing::warn!(task_id = %task_id.0, context, "spawn exit guard skipped missing task");
        return;
    };
    if spawn_exit_state_is_allowed(&snapshot) {
        return;
    }

    let mut reason_to_log: Option<String> = None;
    let persist_result = mutate_and_persist(store, task_id, |state| {
        if spawn_exit_state_is_allowed(state) {
            return;
        }
        let reason = spawn_exit_invariant_reason(context, state);
        state.status = TaskStatus::Failed;
        state.failure_kind = Some(TaskFailureKind::Task);
        state.error = Some(reason.clone());
        state.scheduler.mark_terminal(&TaskStatus::Failed);
        state.rounds.push(RoundResult::new(
            state.turn.saturating_add(1),
            "spawn_exit_guard",
            "nonterminal_exit",
            Some(reason.clone()),
            None,
            None,
        ));
        reason_to_log = Some(reason);
    })
    .await;

    match persist_result {
        Ok(()) => {
            if let Some(reason) = reason_to_log {
                tracing::error!(task_id = %task_id.0, context, reason = %reason);
                log_task_failure_event(events, task_id, &reason).await;
                store.log_event(crate::event_replay::TaskEvent::Failed {
                    task_id: task_id.0.clone(),
                    ts: crate::event_replay::now_ts(),
                    reason,
                });
            }
        }
        Err(error) => {
            tracing::error!(
                task_id = %task_id.0,
                context,
                "failed to persist spawn exit guard terminal state: {error:#}"
            );
        }
    }

    if let Some(final_state) = store.get(task_id) {
        debug_assert!(
            spawn_exit_state_is_allowed(&final_state),
            "{}",
            spawn_exit_invariant_reason(context, &final_state)
        );
    }
}
