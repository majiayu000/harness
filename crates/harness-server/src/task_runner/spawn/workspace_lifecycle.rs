use std::path::PathBuf;

use harness_core::types::{Decision, Event, SessionId};

use super::super::state::{RoundResult, TaskState};
use super::super::store::{mutate_and_persist, TaskStore};
use super::super::types::{TaskFailureKind, TaskId, TaskKind, TaskStatus};

/// Returns true when `external_id` marks this as an issue- or PR-keyed task.
/// Issue/PR-keyed workspaces are reused across consecutive tasks (issue #969).
pub(crate) fn is_issue_pr_task(eid: Option<&str>) -> bool {
    eid.is_some_and(|id| id.starts_with("issue:") || id.starts_with("pr:"))
}

fn is_issue_pr_task_state(state: &TaskState) -> bool {
    is_issue_pr_task(state.external_id.as_deref())
        || state.issue.is_some()
        || matches!(state.task_kind, TaskKind::Issue | TaskKind::Pr)
}

pub(crate) fn should_remove_workspace_after_task(
    state: Option<&TaskState>,
    auto_cleanup: bool,
) -> bool {
    let Some(state) = state else {
        return auto_cleanup;
    };
    if !state.status.is_terminal() {
        return false;
    }
    if state.status == TaskStatus::Failed {
        return true;
    }
    auto_cleanup && !is_issue_pr_task_state(state)
}

pub(crate) fn should_abort_after_abort_handle_registration(
    state: &TaskState,
    handle_finished: bool,
) -> bool {
    state.status.is_terminal() && !handle_finished
}

pub(crate) async fn log_task_failure_event(
    events: &harness_observe::event_store::EventStore,
    task_id: &TaskId,
    reason: &str,
) {
    let mut event = Event::new(
        SessionId::new(),
        "task_failure",
        "task_runner",
        Decision::Block,
    );
    event.reason = Some(reason.to_string());
    event.detail = Some(format!("task_id={}", task_id.0));
    if let Err(e) = events.log(&event).await {
        tracing::warn!("failed to log task_failure event for {task_id:?}: {e}");
    }
}

/// Record a task failure, marking the task as failed.
pub(crate) async fn record_task_failure(
    store: &TaskStore,
    events: &harness_observe::event_store::EventStore,
    task_id: &TaskId,
    reason: String,
) {
    log_task_failure_event(events, task_id, &reason).await;
    store.log_event(crate::event_replay::TaskEvent::Failed {
        task_id: task_id.0.clone(),
        ts: crate::event_replay::now_ts(),
        reason: reason.clone(),
    });
    if let Err(e) = mutate_and_persist(store, task_id, |s| {
        s.status = TaskStatus::Failed;
        s.failure_kind = Some(TaskFailureKind::Task);
        s.error = Some(reason);
        s.scheduler.mark_terminal(&TaskStatus::Failed);
    })
    .await
    {
        tracing::error!("failed to persist task failure for {task_id:?}: {e}");
    }
}

pub(crate) async fn record_workspace_lifecycle_failure(
    store: &TaskStore,
    events: &harness_observe::event_store::EventStore,
    task_id: &TaskId,
    workspace_path: PathBuf,
    reason: String,
) {
    log_task_failure_event(events, task_id, &reason).await;
    store.log_event(crate::event_replay::TaskEvent::Failed {
        task_id: task_id.0.clone(),
        ts: crate::event_replay::now_ts(),
        reason: reason.clone(),
    });
    if let Err(e) = mutate_and_persist(store, task_id, |s| {
        s.status = TaskStatus::Failed;
        s.failure_kind = Some(TaskFailureKind::WorkspaceLifecycle);
        s.error = Some(reason.clone());
        s.workspace_path = Some(workspace_path.clone());
        s.scheduler.mark_terminal(&TaskStatus::Failed);
        s.rounds.push(RoundResult::new(
            s.turn.saturating_add(1),
            "workspace_lifecycle",
            "missing_workspace",
            Some(reason.clone()),
            None,
            None,
        ));
    })
    .await
    {
        tracing::error!("failed to persist workspace lifecycle failure for {task_id:?}: {e}");
    }
}
