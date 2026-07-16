use super::declarative::DeclarativeWorkflowDefinition;
use super::model::{WorkflowCommand, WorkflowCommandType, WorkflowDecision, WorkflowInstance};
use harness_core::config::workflow::DeclaredProgressMode;
use serde_json::json;

pub const DECLARATIVE_SUBMISSION_DECISION: &str = "submit_declarative_workflow";

pub fn build_declarative_submission_decision(
    definition: &DeclarativeWorkflowDefinition,
    instance: &WorkflowInstance,
) -> anyhow::Result<WorkflowDecision> {
    if instance.definition_id != definition.policy().id
        || instance.definition_version != definition.definition_version()
        || instance.state != "__submission__"
    {
        anyhow::bail!(
            "declarative submission instance does not match definition '{}@{}'",
            definition.policy().id,
            definition.definition_version()
        );
    }
    let target = definition.policy().initial.as_str();
    let commands = commands_for_target(
        definition,
        target,
        format!("{}:{}:submit", instance.id, target),
    )?;
    let mut decision = WorkflowDecision::new(
        &instance.id,
        "__submission__",
        DECLARATIVE_SUBMISSION_DECISION,
        target,
        format!("declarative workflow entered initial state '{target}'"),
    )
    .high_confidence();
    decision.commands = commands;
    Ok(decision)
}

fn commands_for_target(
    definition: &DeclarativeWorkflowDefinition,
    target: &str,
    dedupe_key: String,
) -> anyhow::Result<Vec<WorkflowCommand>> {
    if target == "blocked" {
        return Ok(vec![
            WorkflowCommand::mark_blocked(
                "declarative workflow entered blocked state",
                format!("{dedupe_key}:block"),
            ),
            WorkflowCommand::new(
                WorkflowCommandType::RequestOperatorAttention,
                format!("{dedupe_key}:operator"),
                json!({ "state": target }),
            ),
        ]);
    }
    if let Some(state) = definition.policy().states.get(target) {
        if let Some(activity) = state.activity.as_deref() {
            return Ok(vec![WorkflowCommand::enqueue_activity(
                activity, dedupe_key,
            )]);
        }
        return match state.progress {
            Some(DeclaredProgressMode::ExternalWait) => Ok(vec![WorkflowCommand::wait(
                format!("declarative workflow is waiting in state '{target}'"),
                dedupe_key,
            )]),
            Some(DeclaredProgressMode::OperatorGate) => Ok(vec![WorkflowCommand::new(
                WorkflowCommandType::RequestOperatorAttention,
                dedupe_key,
                json!({ "state": target }),
            )]),
            None => anyhow::bail!("declarative target state '{target}' has no progress driver"),
        };
    }
    let class = definition
        .policy()
        .terminal
        .get(target)
        .ok_or_else(|| anyhow::anyhow!("declarative target state '{target}' is undeclared"))?;
    let command_type = match class.as_str() {
        "succeeded" => WorkflowCommandType::MarkDone,
        "failed" => WorkflowCommandType::MarkFailed,
        "cancelled" => WorkflowCommandType::MarkCancelled,
        _ => anyhow::bail!("declarative terminal state '{target}' has invalid class '{class}'"),
    };
    Ok(vec![WorkflowCommand::new(
        command_type,
        dedupe_key,
        json!({ "state": target }),
    )])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::model::WorkflowSubject;
    use harness_core::config::workflow::{
        DeclaredState, WorkflowActivityPolicy, WorkflowDefinitionPolicy,
    };
    use std::collections::BTreeMap;

    fn definition(initial: &str) -> DeclarativeWorkflowDefinition {
        let policy = WorkflowDefinitionPolicy {
            id: "declarative_submission_test".to_string(),
            initial: initial.to_string(),
            states: BTreeMap::from([
                (
                    "working".to_string(),
                    DeclaredState {
                        activity: Some("perform_work".to_string()),
                        on_success: Some("done".to_string()),
                        ..DeclaredState::default()
                    },
                ),
                (
                    "blocked".to_string(),
                    DeclaredState {
                        progress: Some(DeclaredProgressMode::OperatorGate),
                        ..DeclaredState::default()
                    },
                ),
            ]),
            terminal: BTreeMap::from([
                ("done".to_string(), "succeeded".to_string()),
                ("failed".to_string(), "failed".to_string()),
                ("cancelled".to_string(), "cancelled".to_string()),
            ]),
            evidence_required: BTreeMap::new(),
            recovery_targets: vec!["working".to_string()],
        };
        super::super::declarative::build_declarative_definition(
            &policy,
            &BTreeMap::from([(
                "perform_work".to_string(),
                WorkflowActivityPolicy::default(),
            )]),
        )
        .expect("definition should compile")
    }

    #[test]
    fn submission_enqueues_the_declared_initial_activity() -> anyhow::Result<()> {
        let definition = definition("working");
        let instance = WorkflowInstance::new(
            definition.policy().id.clone(),
            definition.definition_version(),
            "__submission__",
            WorkflowSubject::new("test", "submission"),
        );

        let decision = build_declarative_submission_decision(&definition, &instance)?;
        assert_eq!(decision.decision, DECLARATIVE_SUBMISSION_DECISION);
        assert_eq!(decision.next_state, "working");
        assert_eq!(
            decision.commands[0].command_type,
            WorkflowCommandType::EnqueueActivity
        );
        assert_eq!(
            decision.commands[0].dedupe_key,
            format!("{}:working:submit", instance.id)
        );
        Ok(())
    }

    #[test]
    fn submission_rejects_a_mismatched_instance() {
        let definition = definition("working");
        let instance = WorkflowInstance::new(
            "wrong_definition",
            definition.definition_version(),
            "__submission__",
            WorkflowSubject::new("test", "submission-mismatch"),
        );

        assert!(build_declarative_submission_decision(&definition, &instance).is_err());
    }
}
