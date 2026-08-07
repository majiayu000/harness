mod builtin_completion;
mod builtin_github_issue;
mod builtin_plan_issue;
mod builtin_pr_feedback;
mod builtin_prompt_task;
mod builtin_quality_gate;
pub(crate) mod declarative_completion;
mod prompt_completion_evidence;
mod runtime_failure;
mod support;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PromptValidationReportEntry {
    pub(crate) command: String,
    pub(crate) exit_code: i64,
}

use self::builtin_github_issue::{
    closed_issue_evidence_from_activity_result, closed_issue_evidence_from_activity_result_value,
    closed_issue_evidence_from_value,
};
pub(crate) use self::builtin_github_issue::{
    pr_binding_verification_blocked_decision, verified_pr_binding_evidence,
};
use self::declarative_completion::{
    definition_pin_blocked_decision, reduce_declarative_completion,
};
pub(crate) use self::prompt_completion_evidence::first_valid_prompt_validation_report;
pub(crate) use self::support::{
    budget_exhausted_blocked_decision, invalid_agent_output_blocked_decision,
};
use super::model::{ActivityResult, WorkflowDecision, WorkflowEvent, WorkflowInstance};
use super::state_registry::{resolve_declarative_definition, DeclarativeDefinitionResolution};
use serde_json::Value;

pub const RUNTIME_JOB_COMPLETED_EVENT: &str = "RuntimeJobCompleted";
pub const GITHUB_ISSUE_PR_DEFINITION_ID: &str = "github_issue_pr";
pub const ISSUE_CLOSED_SIGNAL: &str = "IssueClosed";
pub const ISSUE_ALREADY_RESOLVED_SIGNAL: &str = "IssueAlreadyResolved";
pub const ISSUE_STATE_ARTIFACT: &str = "issue_state";
pub const SCOPE_TOO_LARGE_SIGNAL: &str = "SCOPE_TOO_LARGE";

pub fn prompt_validation_report_has_nonzero_exit(result: &ActivityResult) -> bool {
    first_valid_prompt_validation_report(result)
        .is_some_and(|entries| entries.iter().any(|entry| entry.exit_code != 0))
}

pub fn activity_result_has_closed_issue_evidence(result: &ActivityResult) -> bool {
    closed_issue_evidence_from_activity_result(result).is_some()
}

pub fn activity_result_value_has_closed_issue_evidence(value: &Value) -> bool {
    closed_issue_evidence_from_activity_result_value(value).is_some()
}

pub fn value_has_closed_issue_evidence(value: &Value) -> bool {
    closed_issue_evidence_from_value(value, ISSUE_STATE_ARTIFACT).is_some()
}

pub fn reduce_runtime_job_completed(
    instance: &WorkflowInstance,
    event: &WorkflowEvent,
) -> anyhow::Result<Option<WorkflowDecision>> {
    if event.event_type != RUNTIME_JOB_COMPLETED_EVENT {
        return Ok(None);
    }

    let result: ActivityResult =
        serde_json::from_value(event.event.get("activity_result").cloned().ok_or_else(|| {
            anyhow::anyhow!("RuntimeJobCompleted event missing activity_result")
        })?)?;

    match resolve_declarative_definition(instance) {
        DeclarativeDefinitionResolution::Resolved(definition) => {
            reduce_declarative_completion(&definition, instance, event, &result)
        }
        DeclarativeDefinitionResolution::PinError(error) => Ok(Some(
            definition_pin_blocked_decision(instance, event, &result, error),
        )),
        DeclarativeDefinitionResolution::NotDeclarative => {
            let reason = format!(
                "workflow definition `{}` is not declarative; runtime job completion requires a registered declarative definition",
                instance.definition_id
            );
            Ok(Some(invalid_agent_output_blocked_decision(
                instance, event, &result, &reason,
            )))
        }
    }
}

#[cfg(test)]
#[path = "reducer/declarative_tests.rs"]
mod declarative_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::model::{
        ActivityArtifact, ActivityErrorKind, ActivityResult, WorkflowCommand, WorkflowCommandType,
        WorkflowEvent, WorkflowInstance, WorkflowSubject,
    };
    use crate::runtime::prompt_task::{PROMPT_TASK_DEFINITION_ID, PROMPT_TASK_IMPLEMENT_ACTIVITY};
    use crate::runtime::validator::{DecisionValidator, ValidationContext};
    use chrono::Utc;
    use serde_json::json;

    #[test]
    fn prompt_task_success_without_validation_evidence_blocks() -> anyhow::Result<()> {
        let instance = WorkflowInstance::new(
            PROMPT_TASK_DEFINITION_ID,
            1,
            "implementing",
            WorkflowSubject::new("prompt", "task-123"),
        );
        let result = ActivityResult::succeeded(
            PROMPT_TASK_IMPLEMENT_ACTIVITY,
            "Prompt implementation completed.",
        );
        let event = WorkflowEvent::new(&instance.id, 1, RUNTIME_JOB_COMPLETED_EVENT, "runtime-1")
            .with_payload(json!({
                "command_id": "command-1",
                "runtime_job_id": "job-1",
                "activity_result": result,
            }));

        let decision = reduce_runtime_job_completed(&instance, &event)?.ok_or_else(|| {
            anyhow::anyhow!("prompt success without validation evidence should block")
        })?;

        assert_eq!(decision.decision, "prompt_completion_evidence_missing");
        assert_eq!(decision.next_state, "blocked");
        assert!(decision
            .commands
            .iter()
            .any(|command| command.command_type == WorkflowCommandType::MarkBlocked));
        assert!(decision
            .commands
            .iter()
            .any(|command| command.command_type == WorkflowCommandType::RequestOperatorAttention));
        DecisionValidator::prompt_task().validate(
            &instance,
            &decision,
            &ValidationContext::new("runtime-1", Utc::now()),
        )?;
        Ok(())
    }

    #[test]
    fn prompt_task_no_change_rationale_reaches_done_through_runtime_reducer() -> anyhow::Result<()>
    {
        let instance = WorkflowInstance::new(
            PROMPT_TASK_DEFINITION_ID,
            1,
            "implementing",
            WorkflowSubject::new("prompt", "task-123"),
        );
        let result = ActivityResult::succeeded(
            PROMPT_TASK_IMPLEMENT_ACTIVITY,
            "No implementation change was needed.",
        )
        .with_artifact(ActivityArtifact::new(
            "no_change_rationale",
            json!("The requested behavior is already present and verified."),
        ));
        let event = WorkflowEvent::new(&instance.id, 1, RUNTIME_JOB_COMPLETED_EVENT, "runtime-1")
            .with_payload(json!({
                "command_id": "command-1",
                "runtime_job_id": "job-1",
                "activity_result": result,
            }));

        let decision = reduce_runtime_job_completed(&instance, &event)?
            .ok_or_else(|| anyhow::anyhow!("prompt completion should produce a decision"))?;

        assert_eq!(decision.decision, "finish_prompt_task");
        assert_eq!(decision.next_state, "done");
        Ok(())
    }

    #[test]
    fn candidate_stall_timeout_records_terminal_evidence_without_failing_workflow(
    ) -> anyhow::Result<()> {
        let instance = WorkflowInstance::new(
            GITHUB_ISSUE_PR_DEFINITION_ID,
            1,
            "implementing",
            WorkflowSubject::new("issue", "issue:1449"),
        )
        .with_id("workflow-1449");
        let command = WorkflowCommand::new(
            WorkflowCommandType::EnqueueActivity,
            "candidate-1",
            json!({
                "activity": "implement_issue",
                "submission_mode": "deferred",
                "candidate": {
                    "candidate_group_id": "workflow-1449:candidate-group:issue-1449",
                    "candidate_id": "workflow-1449:candidate-group:issue-1449:c1",
                    "candidate_index": 1,
                    "candidate_count": 2,
                },
            }),
        );
        let result = ActivityResult::failed(
            "implement_issue",
            "Candidate stalled before completion.",
            "Agent stream stalled: no output for 300s",
        )
        .with_error_kind(ActivityErrorKind::Timeout);
        let event = WorkflowEvent::new(
            &instance.id,
            1,
            RUNTIME_JOB_COMPLETED_EVENT,
            "runtime-worker",
        )
        .with_payload(json!({
            "command_id": "command-c1",
            "runtime_job_id": "job-c1",
            "command": command,
            "activity_result": result,
        }));

        let decision = reduce_runtime_job_completed(&instance, &event)?
            .ok_or_else(|| anyhow::anyhow!("candidate timeout should produce a decision"))?;

        assert_eq!(decision.decision, "record_deferred_candidate_stalled");
        assert_eq!(decision.next_state, "implementing");
        assert!(
            decision.commands.is_empty(),
            "candidate timeouts must not fail or retry the parent workflow"
        );
        let candidate_evidence = decision
            .evidence
            .iter()
            .find(|evidence| evidence.kind == "candidate_terminal")
            .ok_or_else(|| anyhow::anyhow!("missing candidate terminal evidence"))?;
        assert!(candidate_evidence.summary.contains("outcome=stalled"));
        assert!(candidate_evidence.summary.contains("runtime_job_id=job-c1"));
        Ok(())
    }
}
