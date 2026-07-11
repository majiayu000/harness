use crate::task_runner::{
    SchedulerAuthorityState, TaskFailureKind, TaskId, TaskPhase, TaskSchedulerState, TaskStatus,
};
use harness_workflow::runtime::{
    WorkflowInstance, WorkflowTerminalState, QUALITY_GATE_DEFINITION_ID,
};
use serde::Serialize;
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RuntimeActiveBucket {
    Running,
    Queued,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct RuntimeWorkflowProjection {
    pub(crate) task_status: TaskStatus,
    pub(crate) failure_kind: Option<TaskFailureKind>,
    pub(crate) phase: TaskPhase,
    pub(crate) scheduler: TaskSchedulerState,
    active_bucket: Option<RuntimeActiveBucket>,
    pub(crate) project_id: Option<String>,
    pub(crate) submission_handle: Option<TaskId>,
    pub(crate) legacy_dedupe_task_handle: Option<TaskId>,
    pub(crate) stopped_state: RuntimeStoppedStateProjection,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub(crate) struct RuntimeStoppedStateProjection {
    pub(crate) blocked_reason: Option<String>,
    pub(crate) unblock_hint: Option<String>,
    pub(crate) failure_reason: Option<String>,
    pub(crate) error_kind: Option<String>,
    pub(crate) retry_hint: Option<String>,
    pub(crate) last_stop: Option<Value>,
    pub(crate) can_unblock: bool,
    pub(crate) can_retry: bool,
}

impl RuntimeWorkflowProjection {
    pub(crate) fn from_workflow(workflow: &WorkflowInstance) -> Self {
        let terminal_state = workflow.terminal_state();
        let task_status = workflow_state_to_task_status(workflow, terminal_state);
        let scheduler = workflow_scheduler_state(&workflow.state, &task_status, terminal_state);
        let active_bucket = workflow_active_bucket(
            &workflow.definition_id,
            &workflow.state,
            &task_status,
            &scheduler,
        );
        Self {
            failure_kind: task_status.is_failure().then_some(TaskFailureKind::Task),
            phase: workflow_state_to_task_phase(&workflow.state, terminal_state),
            task_status,
            scheduler,
            active_bucket,
            project_id: runtime_string_field(&workflow.data, "project_id"),
            submission_handle: runtime_submission_handle(&workflow.data),
            legacy_dedupe_task_handle: legacy_dedupe_task_handle(&workflow.data),
            stopped_state: RuntimeStoppedStateProjection::from_workflow(workflow),
        }
    }

    pub(crate) fn active_bucket(&self) -> Option<RuntimeActiveBucket> {
        self.active_bucket
    }
}

impl RuntimeStoppedStateProjection {
    pub(crate) fn from_workflow(workflow: &WorkflowInstance) -> Self {
        let last_stop = structured_last_stop(&workflow.data);
        let error_kind = stopped_string_field(&workflow.data, "error_kind").or_else(|| {
            last_stop
                .as_ref()
                .and_then(|value| value.get("error_kind"))
                .and_then(trimmed_string)
        });
        Self {
            blocked_reason: stopped_string_field(&workflow.data, "blocked_reason"),
            unblock_hint: stopped_string_field(&workflow.data, "unblock_hint"),
            failure_reason: first_stopped_string_field(
                &workflow.data,
                &["failure_reason", "previous_error", "last_error", "error"],
            ),
            retry_hint: stopped_string_field(&workflow.data, "retry_hint"),
            can_unblock: workflow.state == "blocked",
            can_retry: workflow.state == "failed" && retryable_error_kind(error_kind.as_deref()),
            error_kind,
            last_stop,
        }
    }
}

pub(crate) fn runtime_string_field(data: &serde_json::Value, field: &str) -> Option<String> {
    data.get(field)
        .and_then(|value| value.as_str())
        .map(ToOwned::to_owned)
}

pub(crate) fn workflow_state_to_task_status(
    workflow: &WorkflowInstance,
    terminal_state: Option<WorkflowTerminalState>,
) -> TaskStatus {
    match terminal_state {
        Some(WorkflowTerminalState::Succeeded) => return TaskStatus::Done,
        Some(WorkflowTerminalState::Failed) => return TaskStatus::Failed,
        Some(WorkflowTerminalState::Cancelled) => return TaskStatus::Cancelled,
        None => {}
    }
    match workflow.state.as_str() {
        "awaiting_dependencies" => TaskStatus::AwaitingDeps,
        "scheduled" | "discovered" => TaskStatus::Pending,
        "planning" => TaskStatus::Planning,
        "checking"
        | "dispatching"
        | "implementing"
        | "inspecting"
        | "planning_batch"
        | "reconciling"
        | "replanning"
        | "scanning"
        | "addressing_feedback" => TaskStatus::Implementing,
        "feedback_found"
        | "idle"
        | "no_actionable_feedback"
        | "pr_open"
        | "local_review_gate"
        | "awaiting_feedback"
        | "quality_gate_pending"
        | "ready_to_merge"
        | "blocked" => TaskStatus::Waiting,
        _ => TaskStatus::Waiting,
    }
}

fn workflow_state_to_task_phase(
    state: &str,
    terminal_state: Option<WorkflowTerminalState>,
) -> TaskPhase {
    if terminal_state.is_some() {
        return TaskPhase::Terminal;
    }
    match state {
        "pr_open"
        | "local_review_gate"
        | "awaiting_feedback"
        | "quality_gate_pending"
        | "ready_to_merge" => TaskPhase::Review,
        "planning" | "replanning" | "blocked" => TaskPhase::Plan,
        _ => TaskPhase::Implement,
    }
}

fn workflow_scheduler_state(
    state: &str,
    status: &TaskStatus,
    terminal_state: Option<WorkflowTerminalState>,
) -> TaskSchedulerState {
    if terminal_state.is_some() {
        let mut scheduler = TaskSchedulerState::queued();
        scheduler.mark_terminal(status);
        return scheduler;
    }
    match state {
        "awaiting_dependencies" => TaskSchedulerState::awaiting_dependencies(),
        "scheduled" | "discovered" => TaskSchedulerState::queued(),
        "checking"
        | "dispatching"
        | "implementing"
        | "inspecting"
        | "planning"
        | "planning_batch"
        | "reconciling"
        | "replanning"
        | "scanning"
        | "addressing_feedback" => TaskSchedulerState {
            authority_state: SchedulerAuthorityState::Running,
            owner: None,
            run_generation: 0,
            recovery_generation: 0,
            lease_expires_at: None,
        },
        "pr_open"
        | "local_review_gate"
        | "awaiting_feedback"
        | "quality_gate_pending"
        | "ready_to_merge"
        | "blocked" => TaskSchedulerState::queued(),
        _ => match status {
            TaskStatus::AwaitingDeps => TaskSchedulerState::awaiting_dependencies(),
            TaskStatus::Pending => TaskSchedulerState::queued(),
            TaskStatus::Done | TaskStatus::Failed | TaskStatus::Cancelled => {
                let mut scheduler = TaskSchedulerState::queued();
                scheduler.mark_terminal(status);
                scheduler
            }
            _ => TaskSchedulerState::queued(),
        },
    }
}

fn workflow_active_bucket(
    definition_id: &str,
    state: &str,
    status: &TaskStatus,
    scheduler: &TaskSchedulerState,
) -> Option<RuntimeActiveBucket> {
    if status.is_terminal()
        || state == "idle"
        || (definition_id == QUALITY_GATE_DEFINITION_ID && state == "pending")
    {
        return None;
    }
    match scheduler.authority_state {
        SchedulerAuthorityState::Running
        | SchedulerAuthorityState::Leased
        | SchedulerAuthorityState::Recovering => Some(RuntimeActiveBucket::Running),
        _ => Some(RuntimeActiveBucket::Queued),
    }
}

fn runtime_submission_handle(data: &serde_json::Value) -> Option<TaskId> {
    trimmed_string_field(data, "submission_id")
        .or_else(|| first_string_array_field(data, "task_ids"))
        .or_else(|| trimmed_string_field(data, "task_id"))
        .map(harness_core::types::TaskId)
}

fn stopped_string_field(data: &serde_json::Value, field: &str) -> Option<String> {
    data.get(field).and_then(trimmed_string)
}

fn first_stopped_string_field(data: &serde_json::Value, fields: &[&str]) -> Option<String> {
    fields
        .iter()
        .find_map(|field| stopped_string_field(data, field))
}

fn structured_last_stop(data: &serde_json::Value) -> Option<Value> {
    data.get("last_stop")
        .filter(|value| value.is_object())
        .cloned()
}

fn retryable_error_kind(error_kind: Option<&str>) -> bool {
    !matches!(
        error_kind.map(str::to_ascii_lowercase).as_deref(),
        Some("fatal" | "configuration")
    )
}

fn legacy_dedupe_task_handle(data: &serde_json::Value) -> Option<TaskId> {
    trimmed_string_field(data, "task_id")
        .or_else(|| last_string_array_field(data, "task_ids"))
        .or_else(|| trimmed_string_field(data, "submission_id"))
        .map(harness_core::types::TaskId)
}

fn trimmed_string_field(data: &serde_json::Value, field: &str) -> Option<String> {
    data.get(field).and_then(trimmed_string)
}

fn first_string_array_field(data: &serde_json::Value, field: &str) -> Option<String> {
    data.get(field)
        .and_then(serde_json::Value::as_array)
        .and_then(|values| values.iter().find_map(trimmed_string))
}

fn last_string_array_field(data: &serde_json::Value, field: &str) -> Option<String> {
    data.get(field)
        .and_then(serde_json::Value::as_array)
        .and_then(|values| values.iter().rev().find_map(trimmed_string))
}

fn trimmed_string(value: &serde_json::Value) -> Option<String> {
    value
        .as_str()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_workflow::runtime::{
        WorkflowInstance, WorkflowSubject, QUALITY_GATE_DEFINITION_ID,
    };

    fn workflow(state: &str, data: serde_json::Value) -> WorkflowInstance {
        workflow_with_definition(
            harness_workflow::runtime::GITHUB_ISSUE_PR_DEFINITION_ID,
            state,
            data,
        )
    }

    fn workflow_with_definition(
        definition_id: &str,
        state: &str,
        data: serde_json::Value,
    ) -> WorkflowInstance {
        WorkflowInstance::new(
            definition_id,
            1,
            state,
            WorkflowSubject::new("issue", "issue:1"),
        )
        .with_data(data)
    }

    #[test]
    fn projection_exposes_stable_submission_handle_and_current_legacy_handle() {
        let workflow = workflow(
            "implementing",
            serde_json::json!({
                "project_id": "/repo",
                "submission_id": "first-submission",
                "task_id": "current-task",
                "task_ids": ["first-submission", "previous-task"],
            }),
        );

        let projection = RuntimeWorkflowProjection::from_workflow(&workflow);

        assert_eq!(
            projection.submission_handle.as_ref().map(TaskId::as_str),
            Some("first-submission")
        );
        assert_eq!(
            projection
                .legacy_dedupe_task_handle
                .as_ref()
                .map(TaskId::as_str),
            Some("current-task")
        );
    }

    #[test]
    fn projection_uses_task_history_when_explicit_task_id_is_missing() {
        let workflow = workflow(
            "implementing",
            serde_json::json!({
                "task_ids": ["first-submission", "current-task"],
            }),
        );

        let projection = RuntimeWorkflowProjection::from_workflow(&workflow);

        assert_eq!(
            projection.submission_handle.as_ref().map(TaskId::as_str),
            Some("first-submission")
        );
        assert_eq!(
            projection
                .legacy_dedupe_task_handle
                .as_ref()
                .map(TaskId::as_str),
            Some("current-task")
        );
    }

    #[test]
    fn projection_marks_active_execution_states_as_running() {
        let workflow = workflow("implementing", serde_json::json!({}));

        let projection = RuntimeWorkflowProjection::from_workflow(&workflow);

        assert_eq!(projection.task_status, TaskStatus::Implementing);
        assert_eq!(
            projection.scheduler.authority_state,
            SchedulerAuthorityState::Running
        );
        assert_eq!(
            projection.active_bucket(),
            Some(RuntimeActiveBucket::Running)
        );
    }

    #[test]
    fn projection_marks_review_wait_states_as_queued_active_work() {
        let workflow = workflow("awaiting_feedback", serde_json::json!({}));

        let projection = RuntimeWorkflowProjection::from_workflow(&workflow);

        assert_eq!(projection.task_status, TaskStatus::Waiting);
        assert_eq!(
            projection.scheduler.authority_state,
            SchedulerAuthorityState::Queued
        );
        assert_eq!(
            projection.active_bucket(),
            Some(RuntimeActiveBucket::Queued)
        );
    }

    #[test]
    fn projection_excludes_terminal_work_from_active_counts() {
        let workflow = workflow("failed", serde_json::json!({}));

        let projection = RuntimeWorkflowProjection::from_workflow(&workflow);

        assert_eq!(projection.task_status, TaskStatus::Failed);
        assert_eq!(
            projection.scheduler.authority_state,
            SchedulerAuthorityState::Failed
        );
        assert_eq!(projection.phase, TaskPhase::Terminal);
        assert_eq!(projection.active_bucket(), None);
    }

    #[test]
    fn projection_uses_definition_specific_terminal_passed_state() {
        let workflow =
            workflow_with_definition(QUALITY_GATE_DEFINITION_ID, "passed", serde_json::json!({}));

        let projection = RuntimeWorkflowProjection::from_workflow(&workflow);

        assert!(workflow.is_terminal());
        assert_eq!(projection.task_status, TaskStatus::Done);
        assert_eq!(
            projection.scheduler.authority_state,
            SchedulerAuthorityState::Done
        );
        assert_eq!(projection.phase, TaskPhase::Terminal);
        assert_eq!(projection.active_bucket(), None);
    }

    #[test]
    fn projection_does_not_treat_passed_as_github_issue_pr_terminal_state() {
        let workflow = workflow("passed", serde_json::json!({}));

        let projection = RuntimeWorkflowProjection::from_workflow(&workflow);

        assert!(!workflow.is_terminal());
        assert_eq!(projection.task_status, TaskStatus::Waiting);
        assert_eq!(
            projection.scheduler.authority_state,
            SchedulerAuthorityState::Queued
        );
        assert_eq!(projection.phase, TaskPhase::Implement);
        assert_eq!(
            projection.active_bucket(),
            Some(RuntimeActiveBucket::Queued)
        );
    }

    #[test]
    fn projection_maps_runtime_execution_states_to_running() {
        for state in [
            "checking",
            "dispatching",
            "implementing",
            "inspecting",
            "planning_batch",
            "reconciling",
            "replanning",
            "scanning",
            "addressing_feedback",
        ] {
            let workflow = workflow(state, serde_json::json!({}));

            let projection = RuntimeWorkflowProjection::from_workflow(&workflow);

            assert_eq!(projection.task_status, TaskStatus::Implementing, "{state}");
            assert_eq!(
                projection.scheduler.authority_state,
                SchedulerAuthorityState::Running,
                "{state}"
            );
            assert_eq!(
                projection.active_bucket(),
                Some(RuntimeActiveBucket::Running),
                "{state}"
            );
        }
    }

    #[test]
    fn projection_preserves_planning_status_but_marks_scheduler_running() {
        let workflow = workflow("planning", serde_json::json!({}));

        let projection = RuntimeWorkflowProjection::from_workflow(&workflow);

        assert_eq!(projection.task_status, TaskStatus::Planning);
        assert_eq!(projection.phase, TaskPhase::Plan);
        assert_eq!(
            projection.scheduler.authority_state,
            SchedulerAuthorityState::Running
        );
        assert_eq!(
            projection.active_bucket(),
            Some(RuntimeActiveBucket::Running)
        );
    }

    #[test]
    fn projection_excludes_idle_workflows_from_active_counts() {
        let workflow = workflow_with_definition(
            harness_workflow::runtime::QUALITY_GATE_DEFINITION_ID,
            "pending",
            serde_json::json!({}),
        );

        let projection = RuntimeWorkflowProjection::from_workflow(&workflow);

        assert_eq!(projection.task_status, TaskStatus::Waiting);
        assert_eq!(
            projection.scheduler.authority_state,
            SchedulerAuthorityState::Queued
        );
        assert_eq!(projection.active_bucket(), None);
    }

    #[test]
    fn projection_exposes_structured_blocked_stop_metadata() {
        let workflow = workflow(
            "blocked",
            serde_json::json!({
                "blocked_reason": "Waiting for maintainer approval.",
                "unblock_hint": "Post the approval comment, then call unblock.",
                "last_stop": {
                    "state": "blocked",
                    "activity": "implement_issue",
                    "runtime_job_id": "job-1",
                    "event_id": 10
                }
            }),
        );

        let projection = RuntimeWorkflowProjection::from_workflow(&workflow);

        assert_eq!(
            projection.stopped_state.blocked_reason.as_deref(),
            Some("Waiting for maintainer approval.")
        );
        assert_eq!(
            projection.stopped_state.unblock_hint.as_deref(),
            Some("Post the approval comment, then call unblock.")
        );
        assert_eq!(
            projection.stopped_state.last_stop.as_ref().unwrap()["activity"],
            "implement_issue"
        );
        assert!(projection.stopped_state.can_unblock);
        assert!(!projection.stopped_state.can_retry);
    }

    #[test]
    fn projection_exposes_retry_eligibility_from_structured_error_kind() {
        let retryable = workflow(
            "failed",
            serde_json::json!({
                "failure_reason": "Runtime transport timed out.",
                "error_kind": "timeout",
                "retry_hint": "Fix the transient condition, then call retry.",
                "last_stop": {
                    "state": "failed",
                    "activity": "implement_issue",
                    "runtime_job_id": "job-2"
                }
            }),
        );
        let configuration = workflow(
            "failed",
            serde_json::json!({
                "failure_reason": "Missing configuration.",
                "error_kind": "configuration"
            }),
        );
        let cancelled = workflow(
            "cancelled",
            serde_json::json!({
                "failure_reason": "Operator cancelled the workflow."
            }),
        );
        let legacy = workflow(
            "failed",
            serde_json::json!({
                "previous_error": "Legacy workflow failed before structured metadata shipped."
            }),
        );

        let retryable = RuntimeWorkflowProjection::from_workflow(&retryable);
        assert_eq!(
            retryable.stopped_state.failure_reason.as_deref(),
            Some("Runtime transport timed out.")
        );
        assert_eq!(
            retryable.stopped_state.error_kind.as_deref(),
            Some("timeout")
        );
        assert!(retryable.stopped_state.can_retry);
        assert!(!retryable.stopped_state.can_unblock);

        let configuration = RuntimeWorkflowProjection::from_workflow(&configuration);
        assert!(!configuration.stopped_state.can_retry);
        assert!(!configuration.stopped_state.can_unblock);

        let cancelled = RuntimeWorkflowProjection::from_workflow(&cancelled);
        assert!(!cancelled.stopped_state.can_retry);
        assert!(!cancelled.stopped_state.can_unblock);

        let legacy = RuntimeWorkflowProjection::from_workflow(&legacy);
        assert_eq!(
            legacy.stopped_state.failure_reason.as_deref(),
            Some("Legacy workflow failed before structured metadata shipped.")
        );
    }

    #[test]
    fn projection_keeps_pr_feedback_outcomes_queued_without_review_phase() {
        for state in ["feedback_found", "no_actionable_feedback"] {
            let workflow = workflow(state, serde_json::json!({}));

            let projection = RuntimeWorkflowProjection::from_workflow(&workflow);

            assert_eq!(projection.task_status, TaskStatus::Waiting, "{state}");
            assert_eq!(projection.phase, TaskPhase::Implement, "{state}");
            assert_eq!(
                projection.scheduler.authority_state,
                SchedulerAuthorityState::Queued,
                "{state}"
            );
            assert_eq!(
                projection.active_bucket(),
                Some(RuntimeActiveBucket::Queued),
                "{state}"
            );
        }
    }
}
