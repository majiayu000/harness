use super::{WorkflowDecisionRejection, WorkflowDecisionRejectionKind};
use crate::runtime::model::{WorkflowCommandType, WorkflowDecision};
use crate::runtime::prompt_task::PROMPT_TASK_IMPLEMENT_ACTIVITY;

pub(super) fn validate_decision(
    decision: &WorkflowDecision,
) -> Result<(), WorkflowDecisionRejection> {
    if decision.observed_state != "implementing" || decision.next_state != "implementing" {
        return Ok(());
    }
    if decision.decision != "continue_prompt_task" {
        return Err(invalid_contract(
            "prompt_task implementing self-transitions require decision continue_prompt_task",
        ));
    }
    let enqueue_commands = decision
        .commands
        .iter()
        .filter(|command| command.command_type == WorkflowCommandType::EnqueueActivity)
        .collect::<Vec<_>>();
    if enqueue_commands.len() != 1 || decision.commands.len() != 1 {
        return Err(invalid_contract(
            "continue_prompt_task requires exactly one EnqueueActivity command",
        ));
    }
    if enqueue_commands[0].activity_name() != Some(PROMPT_TASK_IMPLEMENT_ACTIVITY) {
        return Err(invalid_contract(
            "continue_prompt_task must enqueue implement_prompt",
        ));
    }
    Ok(())
}

fn invalid_contract(message: &str) -> WorkflowDecisionRejection {
    WorkflowDecisionRejection::new(
        WorkflowDecisionRejectionKind::InvalidDecisionContract,
        message,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::model::{
        WorkflowCommand, WorkflowCommandType, WorkflowDecision, WorkflowInstance, WorkflowSubject,
    };
    use crate::runtime::validator::{DecisionValidator, ValidationContext};
    use chrono::Utc;
    use serde_json::json;

    fn instance() -> WorkflowInstance {
        WorkflowInstance::new(
            "prompt_task",
            1,
            "implementing",
            WorkflowSubject::new("prompt", "task-1"),
        )
    }

    #[test]
    fn prompt_task_validator_accepts_exact_continuation_contract() {
        let instance = instance();
        let decision = WorkflowDecision::new(
            &instance.id,
            "implementing",
            "continue_prompt_task",
            "implementing",
            "continue",
        )
        .with_command(WorkflowCommand::enqueue_activity(
            PROMPT_TASK_IMPLEMENT_ACTIVITY,
            "prompt-task:task-1:attempt:2",
        ));
        DecisionValidator::prompt_task()
            .validate(
                &instance,
                &decision,
                &ValidationContext::new("runtime", Utc::now()),
            )
            .expect("valid continuation should pass");
    }

    #[test]
    fn prompt_task_validator_rejects_arbitrary_self_transition() {
        let instance = instance();
        let decision = WorkflowDecision::new(
            &instance.id,
            "implementing",
            "wait_somehow",
            "implementing",
            "continue",
        )
        .with_command(WorkflowCommand::enqueue_activity(
            PROMPT_TASK_IMPLEMENT_ACTIVITY,
            "prompt-task:task-1:attempt:2",
        ));
        let error = DecisionValidator::prompt_task()
            .validate(
                &instance,
                &decision,
                &ValidationContext::new("runtime", Utc::now()),
            )
            .expect_err("arbitrary self-transition must fail");
        assert_eq!(
            error.kind,
            WorkflowDecisionRejectionKind::InvalidDecisionContract
        );
    }

    #[test]
    fn prompt_task_validator_rejects_missing_wait_wrong_and_multiple_commands() {
        let instance = instance();
        let cases = [
            WorkflowDecision::new(
                &instance.id,
                "implementing",
                "continue_prompt_task",
                "implementing",
                "continue",
            ),
            WorkflowDecision::new(
                &instance.id,
                "implementing",
                "continue_prompt_task",
                "implementing",
                "continue",
            )
            .with_command(WorkflowCommand::wait("wait", "wait-1")),
            WorkflowDecision::new(
                &instance.id,
                "implementing",
                "continue_prompt_task",
                "implementing",
                "continue",
            )
            .with_command(WorkflowCommand::enqueue_activity(
                "wrong_activity",
                "wrong-1",
            )),
            WorkflowDecision::new(
                &instance.id,
                "implementing",
                "continue_prompt_task",
                "implementing",
                "continue",
            )
            .with_command(WorkflowCommand::enqueue_activity(
                PROMPT_TASK_IMPLEMENT_ACTIVITY,
                "implement-1",
            ))
            .with_command(WorkflowCommand::new(
                WorkflowCommandType::EnqueueActivity,
                "implement-2",
                json!({ "activity": PROMPT_TASK_IMPLEMENT_ACTIVITY }),
            )),
        ];
        for decision in cases {
            let error = DecisionValidator::prompt_task()
                .validate(
                    &instance,
                    &decision,
                    &ValidationContext::new("runtime", Utc::now()),
                )
                .expect_err("invalid continuation contract must fail closed");
            assert_eq!(
                error.kind,
                WorkflowDecisionRejectionKind::InvalidDecisionContract
            );
        }
    }
}
