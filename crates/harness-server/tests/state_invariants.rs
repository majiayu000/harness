//! Cross-state-machine invariants for harness task and workflow state.
//!
//! Three independent enums describe different views of the same task and are
//! persisted independently:
//!
//! * [`TaskStatus`](harness_server::task_runner::TaskStatus) — execution phase,
//!   persisted in the `tasks.status` column.
//! * [`SchedulerAuthorityState`](harness_server::task_runner::SchedulerAuthorityState)
//!   — scheduler ownership and recovery, persisted in `tasks.scheduler_state`.
//! * [`IssueLifecycleState`](harness_workflow::issue_lifecycle::IssueLifecycleState)
//!   — issue → PR → merge lifecycle, persisted in JSONB on
//!   `workflow_instances.data`.
//!
//! The database does not enforce coupling between the three. These tests act as
//! a typed fence so that adding or renaming a variant in one enum does not
//! silently drift away from the others. Each helper uses an exhaustive `match`
//! arm: any new variant breaks compilation here, forcing the contributor to
//! decide explicitly how the new variant relates to the rest.

use harness_server::task_runner::{SchedulerAuthorityState, TaskStatus};
use harness_workflow::issue_lifecycle::IssueLifecycleState;

fn all_task_statuses() -> Vec<TaskStatus> {
    vec![
        TaskStatus::Pending,
        TaskStatus::AwaitingDeps,
        TaskStatus::Triaging,
        TaskStatus::Planning,
        TaskStatus::Implementing,
        TaskStatus::ReviewGenerating,
        TaskStatus::ReviewWaiting,
        TaskStatus::PlannerGenerating,
        TaskStatus::PlannerWaiting,
        TaskStatus::AgentReview,
        TaskStatus::Waiting,
        TaskStatus::Reviewing,
        TaskStatus::Done,
        TaskStatus::Failed,
        TaskStatus::Cancelled,
    ]
}

fn task_status_tag(status: &TaskStatus) -> &'static str {
    match status {
        TaskStatus::Pending => "pending",
        TaskStatus::AwaitingDeps => "awaiting_deps",
        TaskStatus::Triaging => "triaging",
        TaskStatus::Planning => "planning",
        TaskStatus::Implementing => "implementing",
        TaskStatus::ReviewGenerating => "review_generating",
        TaskStatus::ReviewWaiting => "review_waiting",
        TaskStatus::PlannerGenerating => "planner_generating",
        TaskStatus::PlannerWaiting => "planner_waiting",
        TaskStatus::AgentReview => "agent_review",
        TaskStatus::Waiting => "waiting",
        TaskStatus::Reviewing => "reviewing",
        TaskStatus::Done => "done",
        TaskStatus::Failed => "failed",
        TaskStatus::Cancelled => "cancelled",
    }
}

fn task_is_terminal(status: &TaskStatus) -> bool {
    match status {
        TaskStatus::Done | TaskStatus::Failed | TaskStatus::Cancelled => true,
        TaskStatus::Pending
        | TaskStatus::AwaitingDeps
        | TaskStatus::Triaging
        | TaskStatus::Planning
        | TaskStatus::Implementing
        | TaskStatus::ReviewGenerating
        | TaskStatus::ReviewWaiting
        | TaskStatus::PlannerGenerating
        | TaskStatus::PlannerWaiting
        | TaskStatus::AgentReview
        | TaskStatus::Waiting
        | TaskStatus::Reviewing => false,
    }
}

fn all_scheduler_states() -> Vec<SchedulerAuthorityState> {
    vec![
        SchedulerAuthorityState::Queued,
        SchedulerAuthorityState::AwaitingDependencies,
        SchedulerAuthorityState::Running,
        SchedulerAuthorityState::RetryBackoff,
        SchedulerAuthorityState::Leased,
        SchedulerAuthorityState::Recovering,
        SchedulerAuthorityState::Done,
        SchedulerAuthorityState::Failed,
        SchedulerAuthorityState::Cancelled,
    ]
}

fn scheduler_tag(state: &SchedulerAuthorityState) -> &'static str {
    match state {
        SchedulerAuthorityState::Queued => "queued",
        SchedulerAuthorityState::AwaitingDependencies => "awaiting_dependencies",
        SchedulerAuthorityState::Running => "running",
        SchedulerAuthorityState::RetryBackoff => "retry_backoff",
        SchedulerAuthorityState::Leased => "leased",
        SchedulerAuthorityState::Recovering => "recovering",
        SchedulerAuthorityState::Done => "done",
        SchedulerAuthorityState::Failed => "failed",
        SchedulerAuthorityState::Cancelled => "cancelled",
    }
}

fn scheduler_is_terminal(state: &SchedulerAuthorityState) -> bool {
    match state {
        SchedulerAuthorityState::Done
        | SchedulerAuthorityState::Failed
        | SchedulerAuthorityState::Cancelled => true,
        SchedulerAuthorityState::Queued
        | SchedulerAuthorityState::AwaitingDependencies
        | SchedulerAuthorityState::Running
        | SchedulerAuthorityState::RetryBackoff
        | SchedulerAuthorityState::Leased
        | SchedulerAuthorityState::Recovering => false,
    }
}

fn all_lifecycle_states() -> Vec<IssueLifecycleState> {
    vec![
        IssueLifecycleState::Discovered,
        IssueLifecycleState::Scheduled,
        IssueLifecycleState::Implementing,
        IssueLifecycleState::PrOpen,
        IssueLifecycleState::AwaitingFeedback,
        IssueLifecycleState::FeedbackClaimed,
        IssueLifecycleState::AddressingFeedback,
        IssueLifecycleState::ReadyToMerge,
        IssueLifecycleState::Blocked,
        IssueLifecycleState::Done,
        IssueLifecycleState::Failed,
        IssueLifecycleState::Cancelled,
    ]
}

fn lifecycle_tag(state: &IssueLifecycleState) -> &'static str {
    match state {
        IssueLifecycleState::Discovered => "discovered",
        IssueLifecycleState::Scheduled => "scheduled",
        IssueLifecycleState::Implementing => "implementing",
        IssueLifecycleState::PrOpen => "pr_open",
        IssueLifecycleState::AwaitingFeedback => "awaiting_feedback",
        IssueLifecycleState::FeedbackClaimed => "feedback_claimed",
        IssueLifecycleState::AddressingFeedback => "addressing_feedback",
        IssueLifecycleState::ReadyToMerge => "ready_to_merge",
        IssueLifecycleState::Blocked => "blocked",
        IssueLifecycleState::Done => "done",
        IssueLifecycleState::Failed => "failed",
        IssueLifecycleState::Cancelled => "cancelled",
    }
}

fn lifecycle_is_terminal(state: &IssueLifecycleState) -> bool {
    match state {
        IssueLifecycleState::Done
        | IssueLifecycleState::Failed
        | IssueLifecycleState::Cancelled => true,
        IssueLifecycleState::Discovered
        | IssueLifecycleState::Scheduled
        | IssueLifecycleState::Implementing
        | IssueLifecycleState::PrOpen
        | IssueLifecycleState::AwaitingFeedback
        | IssueLifecycleState::FeedbackClaimed
        | IssueLifecycleState::AddressingFeedback
        | IssueLifecycleState::ReadyToMerge
        | IssueLifecycleState::Blocked => false,
    }
}

#[test]
fn task_status_serializes_to_expected_snake_case_tag() {
    for status in all_task_statuses() {
        let tag = task_status_tag(&status);
        let json = serde_json::to_value(&status).expect("serialize TaskStatus");
        assert_eq!(
            json.as_str(),
            Some(tag),
            "TaskStatus::{:?} expected wire tag `{}`, got {:?}",
            status,
            tag,
            json,
        );
        let parsed: TaskStatus =
            serde_json::from_str(&format!("\"{tag}\"")).expect("deserialize TaskStatus");
        assert_eq!(parsed, status, "round-trip failed for tag `{tag}`");
    }
}

#[test]
fn scheduler_authority_state_serializes_to_expected_snake_case_tag() {
    for state in all_scheduler_states() {
        let tag = scheduler_tag(&state);
        let json = serde_json::to_value(&state).expect("serialize SchedulerAuthorityState");
        assert_eq!(
            json.as_str(),
            Some(tag),
            "SchedulerAuthorityState::{:?} expected wire tag `{}`, got {:?}",
            state,
            tag,
            json,
        );
        let parsed: SchedulerAuthorityState =
            serde_json::from_str(&format!("\"{tag}\"")).expect("deserialize");
        assert_eq!(parsed, state, "round-trip failed for tag `{tag}`");
    }
}

#[test]
fn issue_lifecycle_state_serializes_to_expected_snake_case_tag() {
    for state in all_lifecycle_states() {
        let tag = lifecycle_tag(&state);
        let json = serde_json::to_value(state).expect("serialize IssueLifecycleState");
        assert_eq!(
            json.as_str(),
            Some(tag),
            "IssueLifecycleState::{:?} expected wire tag `{}`, got {:?}",
            state,
            tag,
            json,
        );
        let parsed: IssueLifecycleState =
            serde_json::from_str(&format!("\"{tag}\"")).expect("deserialize");
        assert_eq!(parsed, state, "round-trip failed for tag `{tag}`");
    }
}

#[test]
fn terminal_wire_tags_agree_across_state_machines() {
    // The wire tags `done`, `failed`, `cancelled` mean the same thing in all
    // three enums. Dashboards and intake correlate by string match across
    // tasks.status and workflow_instances.data, so a divergent terminal tag
    // (e.g. someone adds `succeeded`) would silently break joins.
    let expected: Vec<&str> = {
        let mut v = vec!["cancelled", "done", "failed"];
        v.sort_unstable();
        v
    };

    let mut task_terminals: Vec<&str> = all_task_statuses()
        .iter()
        .filter(|s| task_is_terminal(s))
        .map(task_status_tag)
        .collect();
    task_terminals.sort_unstable();
    assert_eq!(
        task_terminals, expected,
        "TaskStatus terminal tags drifted from the shared set"
    );

    let mut scheduler_terminals: Vec<&str> = all_scheduler_states()
        .iter()
        .filter(|s| scheduler_is_terminal(s))
        .map(scheduler_tag)
        .collect();
    scheduler_terminals.sort_unstable();
    assert_eq!(
        scheduler_terminals, expected,
        "SchedulerAuthorityState terminal tags drifted from the shared set"
    );

    let mut lifecycle_terminals: Vec<&str> = all_lifecycle_states()
        .iter()
        .filter(|s| lifecycle_is_terminal(s))
        .map(lifecycle_tag)
        .collect();
    lifecycle_terminals.sort_unstable();
    assert_eq!(
        lifecycle_terminals, expected,
        "IssueLifecycleState terminal tags drifted from the shared set"
    );
}

#[test]
fn task_status_is_terminal_helper_matches_local_classification() {
    // Sanity-check that the public `TaskStatus::is_terminal()` agrees with the
    // local classification used by the rest of this test file. If they ever
    // disagree, this test file's invariants are based on a stale model and
    // every other assertion in this file becomes meaningless.
    for status in all_task_statuses() {
        assert_eq!(
            status.is_terminal(),
            task_is_terminal(&status),
            "TaskStatus::is_terminal disagrees with the local helper for {:?}",
            status,
        );
    }
}

#[test]
fn task_status_sql_filters_match_restart_classification() {
    let expected_terminal: Vec<&str> = all_task_statuses()
        .iter()
        .filter(|status| task_is_terminal(status))
        .map(task_status_tag)
        .collect();
    assert_eq!(
        TaskStatus::terminal_statuses(),
        expected_terminal.as_slice(),
        "TaskStatus::terminal_statuses must be derived from the terminal classification",
    );

    let expected_resumable: Vec<&str> = all_task_statuses()
        .iter()
        .filter(|status| status.is_resumable_after_restart())
        .map(task_status_tag)
        .collect();
    assert_eq!(
        TaskStatus::resumable_statuses(),
        expected_resumable.as_slice(),
        "TaskStatus::resumable_statuses must be derived from the restart classification",
    );
}
