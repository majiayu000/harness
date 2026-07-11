//! Operator recovery for stopped runtime workflows (GH-1567).
//!
//! Blocked and failed GitHub-issue workflow instances would otherwise stay
//! `blocked`/`failed` forever, and the coverage gate treats those states as
//! covered, so the poller skips the issue indefinitely. These operator actions
//! move a stopped instance back into a dispatchable state (`planning`) and
//! re-enqueue the plan activity, which clears the coverage hold on the next
//! intake tick without any direct database surgery.

use harness_workflow::runtime::{
    WorkflowCommand, WorkflowCommandType, WorkflowDecision, WorkflowInstance, WorkflowRuntimeStore,
    ISSUE_PLAN_ACTIVITY,
};
use serde_json::{json, Value};
use std::fmt;

use super::{commit_runtime_decision, GITHUB_ISSUE_PR_DEFINITION_ID};

/// Stop-metadata fields written by the runtime worker (GH-1573). They are the
/// "active" reason fields cleared once an operator recovers the workflow; the
/// historical `last_stop` object is preserved as evidence.
const ACTIVE_STOP_FIELDS: &[&str] = &[
    "blocked_reason",
    "unblock_hint",
    "failure_reason",
    "retry_hint",
    "error_kind",
];

/// Target state used to re-dispatch a recovered issue workflow. Planning is the
/// earliest safe re-dispatch state accepted by the `github_issue_pr` definition;
/// re-planning re-evaluates the issue instead of resuming mid-implementation.
const RECOVERY_STATE: &str = "planning";

#[derive(Debug, Clone)]
pub(crate) enum RuntimeRecoverOutcome {
    /// The instance was moved back to a dispatchable state.
    Recovered {
        instance: Box<WorkflowInstance>,
        previous_state: String,
    },
    /// The instance exists but is not in a state this action accepts.
    WrongState { current_state: String },
    /// Retry was requested but the recorded failure is not retryable.
    NotRetryable { error_kind: String },
    /// No instance matched the workflow id.
    NotFound,
}

#[derive(Debug)]
pub(crate) enum RuntimeRecoverError {
    UnsupportedDefinition { definition_id: String },
    Store(anyhow::Error),
}

impl fmt::Display for RuntimeRecoverError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedDefinition { definition_id } => write!(
                formatter,
                "workflow definition `{definition_id}` does not support operator recovery"
            ),
            Self::Store(error) => write!(formatter, "{error}"),
        }
    }
}

impl std::error::Error for RuntimeRecoverError {}

impl From<anyhow::Error> for RuntimeRecoverError {
    fn from(error: anyhow::Error) -> Self {
        Self::Store(error)
    }
}

/// Operator unblock: only valid from `blocked`.
pub(crate) async fn unblock_submission_by_workflow_id(
    store: &WorkflowRuntimeStore,
    workflow_id: &str,
    reason: &str,
) -> Result<RuntimeRecoverOutcome, RuntimeRecoverError> {
    recover_submission(store, workflow_id, reason, RecoverKind::Unblock).await
}

/// Operator retry: only valid from `failed`, and rejects non-retryable failures.
pub(crate) async fn retry_submission_by_workflow_id(
    store: &WorkflowRuntimeStore,
    workflow_id: &str,
    reason: &str,
) -> Result<RuntimeRecoverOutcome, RuntimeRecoverError> {
    recover_submission(store, workflow_id, reason, RecoverKind::Retry).await
}

#[derive(Debug, Clone, Copy)]
enum RecoverKind {
    Unblock,
    Retry,
}

impl RecoverKind {
    fn required_state(self) -> &'static str {
        match self {
            Self::Unblock => "blocked",
            Self::Retry => "failed",
        }
    }

    fn decision_name(self) -> &'static str {
        match self {
            Self::Unblock => "unblock_issue_submission",
            Self::Retry => "retry_issue_submission",
        }
    }

    fn event_type(self) -> &'static str {
        match self {
            Self::Unblock => "IssueSubmissionUnblocked",
            Self::Retry => "IssueSubmissionRetried",
        }
    }

    fn command_prefix(self) -> &'static str {
        match self {
            Self::Unblock => "issue-unblock",
            Self::Retry => "issue-retry",
        }
    }
}

async fn recover_submission(
    store: &WorkflowRuntimeStore,
    workflow_id: &str,
    reason: &str,
    kind: RecoverKind,
) -> Result<RuntimeRecoverOutcome, RuntimeRecoverError> {
    let Some(instance) = store.get_instance(workflow_id).await? else {
        return Ok(RuntimeRecoverOutcome::NotFound);
    };
    if instance.definition_id != GITHUB_ISSUE_PR_DEFINITION_ID {
        return Err(RuntimeRecoverError::UnsupportedDefinition {
            definition_id: instance.definition_id,
        });
    }
    if instance.state != kind.required_state() {
        return Ok(RuntimeRecoverOutcome::WrongState {
            current_state: instance.state,
        });
    }
    if matches!(kind, RecoverKind::Retry) {
        if let Some(error_kind) = non_retryable_error_kind(&instance.data) {
            return Ok(RuntimeRecoverOutcome::NotRetryable { error_kind });
        }
    }

    let previous_state = instance.state.clone();
    let operator_reason = normalize_reason(reason);
    let event = store
        .append_event(
            &instance.id,
            kind.event_type(),
            "workflow_runtime_submission",
            json!({
                "execution_path": super::EXECUTION_PATH_WORKFLOW_RUNTIME,
                "previous_state": previous_state,
                "reason": operator_reason,
            }),
        )
        .await?;

    let plan_command = WorkflowCommand::new(
        WorkflowCommandType::EnqueueActivity,
        format!("{}:{}:plan", kind.command_prefix(), event.id),
        json!({
            "activity": ISSUE_PLAN_ACTIVITY,
            "dispatch_gate": { "reason": kind.decision_name() },
        }),
    );
    let decision = WorkflowDecision::new(
        &instance.id,
        &instance.state,
        kind.decision_name(),
        RECOVERY_STATE,
        &operator_reason,
    )
    .with_command(plan_command)
    .high_confidence();

    // `commit_runtime_decision` persists this cleared data as the accepted
    // instance data; it only additionally merges a `last_decision` marker, which
    // does not re-introduce the stop fields, so no second write is needed.
    let recovered_data = clear_active_stop_fields(instance.data.clone(), kind, &operator_reason);
    let recovered =
        commit_runtime_decision(store, instance, decision, event.id, Some(recovered_data)).await?;

    Ok(RuntimeRecoverOutcome::Recovered {
        instance: Box::new(recovered),
        previous_state,
    })
}

/// Returns the recorded `error_kind` when it is a failure class the first
/// implementation refuses to auto-retry (see GH-1567 tech spec). Missing or
/// unknown kinds are treated as retryable.
fn non_retryable_error_kind(data: &Value) -> Option<String> {
    let error_kind = data
        .get("error_kind")
        .or_else(|| {
            data.get("last_stop")
                .and_then(|stop| stop.get("error_kind"))
        })
        .and_then(Value::as_str)?;
    matches!(error_kind, "fatal" | "configuration").then(|| error_kind.to_string())
}

fn normalize_reason(reason: &str) -> String {
    let trimmed = reason.trim();
    if trimmed.is_empty() {
        "operator recovered the runtime issue submission".to_string()
    } else {
        trimmed.to_string()
    }
}

/// Clears the active stop-reason fields while preserving `last_stop` evidence,
/// and records a structured recovery marker.
fn clear_active_stop_fields(mut data: Value, kind: RecoverKind, reason: &str) -> Value {
    if let Some(object) = data.as_object_mut() {
        for field in ACTIVE_STOP_FIELDS {
            object.remove(*field);
        }
        object.insert(
            "last_recovery".to_string(),
            json!({
                "action": kind.decision_name(),
                "reason": reason,
            }),
        );
    }
    data
}
