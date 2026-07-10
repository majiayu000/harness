use super::{
    insert_event_tx, select_instance_for_update_tx, upsert_instance_tx, WorkflowInstance,
    WorkflowRuntimeStore,
};
use crate::runtime::model::ActivityErrorKind;
use crate::runtime::reducer::GITHUB_ISSUE_PR_DEFINITION_ID;
use serde_json::{json, Value};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkflowRuntimeRecoveryAction {
    Unblock,
    Retry,
}

impl WorkflowRuntimeRecoveryAction {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unblock => "unblock",
            Self::Retry => "retry",
        }
    }

    fn expected_state(self) -> &'static str {
        match self {
            Self::Unblock => "blocked",
            Self::Retry => "failed",
        }
    }

    fn success_status(self) -> &'static str {
        match self {
            Self::Unblock => "unblocked",
            Self::Retry => "retried",
        }
    }

    fn event_type(self) -> &'static str {
        match self {
            Self::Unblock => "WorkflowRuntimeUnblocked",
            Self::Retry => "WorkflowRuntimeRetried",
        }
    }
}

pub struct WorkflowRuntimeRecoveryRequest<'a> {
    pub workflow_id: &'a str,
    pub action: WorkflowRuntimeRecoveryAction,
    pub reason: &'a str,
    pub actor: &'a str,
    pub next_state: &'a str,
}

#[derive(Debug, Clone, PartialEq)]
pub enum WorkflowRuntimeRecoveryOutcome {
    Recovered {
        workflow: WorkflowInstance,
        previous_state: String,
        event_id: String,
    },
    WrongState {
        workflow: WorkflowInstance,
    },
    NonRetryableFailure {
        workflow: WorkflowInstance,
        error_kind: ActivityErrorKind,
    },
    UnsupportedDefinition {
        workflow: WorkflowInstance,
    },
    NotFound,
}

impl WorkflowRuntimeStore {
    pub async fn recover_stopped_instance(
        &self,
        request: WorkflowRuntimeRecoveryRequest<'_>,
    ) -> anyhow::Result<WorkflowRuntimeRecoveryOutcome> {
        let mut tx = self.pool.begin().await?;
        let Some(mut instance) =
            select_instance_for_update_tx(&mut tx, request.workflow_id).await?
        else {
            tx.commit().await?;
            return Ok(WorkflowRuntimeRecoveryOutcome::NotFound);
        };

        let previous_state = instance.state.clone();
        if instance.definition_id != GITHUB_ISSUE_PR_DEFINITION_ID {
            tx.commit().await?;
            return Ok(WorkflowRuntimeRecoveryOutcome::UnsupportedDefinition {
                workflow: instance,
            });
        }

        if previous_state != request.action.expected_state() {
            tx.commit().await?;
            return Ok(WorkflowRuntimeRecoveryOutcome::WrongState { workflow: instance });
        }

        if request.action == WorkflowRuntimeRecoveryAction::Retry {
            if let Some(error_kind) = stopped_error_kind(&instance.data).filter(|kind| {
                matches!(
                    kind,
                    ActivityErrorKind::Fatal | ActivityErrorKind::Configuration
                )
            }) {
                tx.commit().await?;
                return Ok(WorkflowRuntimeRecoveryOutcome::NonRetryableFailure {
                    workflow: instance,
                    error_kind,
                });
            }
        }

        let event = insert_event_tx(
            &mut tx,
            &instance.id,
            request.action.event_type(),
            "workflow_runtime_operator_action",
            json!({
                "action": request.action.as_str(),
                "status": request.action.success_status(),
                "reason": request.reason,
                "actor": request.actor,
                "previous_state": previous_state,
                "state": request.next_state,
            }),
        )
        .await?;

        instance.state = request.next_state.to_string();
        instance.version = instance.version.saturating_add(1);
        instance.lease = None;
        persist_operator_recovery_data(
            &mut instance,
            request.action,
            request.reason,
            request.actor,
            &previous_state,
            request.next_state,
            &event.id,
        )?;
        upsert_instance_tx(&mut tx, &instance).await?;
        tx.commit().await?;

        Ok(WorkflowRuntimeRecoveryOutcome::Recovered {
            workflow: instance,
            previous_state,
            event_id: event.id,
        })
    }
}

fn persist_operator_recovery_data(
    instance: &mut WorkflowInstance,
    action: WorkflowRuntimeRecoveryAction,
    reason: &str,
    actor: &str,
    previous_state: &str,
    state: &str,
    event_id: &str,
) -> anyhow::Result<()> {
    if !instance.data.is_object() {
        instance.data = json!({});
    }
    let data = instance
        .data
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("workflow instance data is not an object"))?;
    data.insert(
        "last_operator_recovery".to_string(),
        json!({
            "action": action.as_str(),
            "reason": reason,
            "actor": actor,
            "previous_state": previous_state,
            "state": state,
            "event_id": event_id,
        }),
    );
    Ok(())
}

fn stopped_error_kind(data: &Value) -> Option<ActivityErrorKind> {
    data.get("error_kind")
        .cloned()
        .or_else(|| data.pointer("/last_stop/error_kind").cloned())
        .and_then(|value| serde_json::from_value(value).ok())
}
