use super::declarative::DeclarativeWorkflowDefinition;
use super::model::{
    ActivityResult, ActivityStatus, WorkflowCommand, WorkflowCommandType, WorkflowDecision,
    WorkflowEvent, WorkflowEvidence, WorkflowInstance,
};
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

pub(super) fn reduce(
    definition: &DeclarativeWorkflowDefinition,
    instance: &WorkflowInstance,
    event: &WorkflowEvent,
    result: &ActivityResult,
) -> anyhow::Result<WorkflowDecision> {
    let Some(state) = definition.policy().states.get(&instance.state) else {
        return Ok(invalid_completion_decision(
            instance,
            event,
            result,
            format!(
                "current declarative state '{}' is not active",
                instance.state
            ),
        ));
    };
    if state.activity.as_deref() != Some(result.activity.as_str()) {
        return Ok(invalid_completion_decision(
            instance,
            event,
            result,
            format!(
                "runtime activity '{}' does not match activity declared for state '{}'",
                result.activity, instance.state
            ),
        ));
    }

    let route = match result.status {
        ActivityStatus::Succeeded => success_route(state, result).ok_or_else(|| {
            anyhow::anyhow!(
                "activity '{}' succeeded in state '{}' without a declared signal or success route",
                result.activity,
                instance.state
            )
        }),
        ActivityStatus::Blocked => Ok((
            state.on_blocked.as_deref().unwrap_or("blocked"),
            "declarative_activity_blocked",
            format!(
                "declarative activity '{}' reported blocked",
                result.activity
            ),
        )),
        ActivityStatus::Failed => Ok((
            state
                .on_failure
                .as_deref()
                .unwrap_or_else(|| terminal_state_for_class(definition, "failed")),
            "declarative_activity_failed",
            format!("declarative activity '{}' failed", result.activity),
        )),
        ActivityStatus::Cancelled => Ok((
            terminal_state_for_class(definition, "cancelled"),
            "declarative_activity_cancelled",
            format!("declarative activity '{}' was cancelled", result.activity),
        )),
    };
    let (target, decision_name, reason) = match route {
        Ok(route) => route,
        Err(error) => {
            return Ok(invalid_completion_decision(
                instance,
                event,
                result,
                error.to_string(),
            ))
        }
    };
    let commands = commands_for_target(
        definition,
        target,
        format!("{}:{}:{}", instance.id, target, event.id),
    )?;
    let mut decision =
        WorkflowDecision::new(&instance.id, &instance.state, decision_name, target, reason)
            .high_confidence();
    decision.commands = commands;
    decision.evidence = completion_evidence(event, result);
    Ok(decision)
}

pub(super) fn definition_version_missing_decision(
    instance: &WorkflowInstance,
    event: &WorkflowEvent,
) -> WorkflowDecision {
    let reason = format!(
        "pinned declarative definition '{}@{}' is unavailable or its full hash does not match",
        instance.definition_id, instance.definition_version
    );
    WorkflowDecision::new(
        &instance.id,
        &instance.state,
        "definition_version_missing",
        "blocked",
        &reason,
    )
    .with_command(WorkflowCommand::mark_blocked(
        &reason,
        format!("definition-version-missing:{}:block", event.id),
    ))
    .with_command(WorkflowCommand::new(
        WorkflowCommandType::RequestOperatorAttention,
        format!("definition-version-missing:{}:operator", event.id),
        json!({ "reason": reason }),
    ))
    .high_confidence()
}

fn success_route<'a>(
    state: &'a harness_core::config::workflow::DeclaredState,
    result: &ActivityResult,
) -> Option<(&'a str, &'static str, String)> {
    let matched = state
        .on_signal
        .iter()
        .filter(|(signal_type, _)| {
            result
                .signals
                .iter()
                .any(|signal| signal.signal_type == signal_type.as_str())
        })
        .collect::<Vec<_>>();
    if let Some((signal_type, target)) = matched.first() {
        let tie = if matched.len() > 1 {
            format!(
                "; {} mapped signals matched and lexicographic precedence selected '{}'",
                matched.len(),
                signal_type
            )
        } else {
            String::new()
        };
        return Some((
            target.as_str(),
            "declarative_signal_transition",
            format!(
                "declared signal '{}' selected target '{}'{}",
                signal_type, target, tie
            ),
        ));
    }
    state.on_success.as_deref().map(|target| {
        (
            target,
            "declarative_activity_succeeded",
            format!("declarative activity '{}' succeeded", result.activity),
        )
    })
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

fn terminal_state_for_class<'a>(
    definition: &'a DeclarativeWorkflowDefinition,
    class: &str,
) -> &'a str {
    definition
        .policy()
        .terminal
        .iter()
        .find_map(|(state, declared_class)| (declared_class == class).then_some(state.as_str()))
        .unwrap_or("blocked")
}

fn invalid_completion_decision(
    instance: &WorkflowInstance,
    event: &WorkflowEvent,
    result: &ActivityResult,
    reason: String,
) -> WorkflowDecision {
    let mut decision = WorkflowDecision::new(
        &instance.id,
        &instance.state,
        "block_invalid_agent_output",
        "blocked",
        &reason,
    )
    .with_command(WorkflowCommand::mark_blocked(
        &reason,
        format!("runtime-completion:{}:invalid-output:block", event.id),
    ))
    .with_command(WorkflowCommand::new(
        WorkflowCommandType::RequestOperatorAttention,
        format!("runtime-completion:{}:invalid-output:operator", event.id),
        json!({ "reason": reason, "activity": result.activity }),
    ))
    .high_confidence();
    decision.evidence = completion_evidence(event, result);
    decision
}

fn completion_evidence(event: &WorkflowEvent, result: &ActivityResult) -> Vec<WorkflowEvidence> {
    let mut evidence = vec![WorkflowEvidence::new(
        "runtime_completion",
        format!("event_id={}; activity={}", event.id, result.activity),
    )];
    evidence.extend(result.artifacts.iter().map(|artifact| {
        WorkflowEvidence::new(&artifact.artifact_type, artifact.artifact.to_string())
    }));
    evidence.extend(
        result
            .signals
            .iter()
            .map(|signal| WorkflowEvidence::new(&signal.signal_type, signal.signal.to_string())),
    );
    evidence.extend(result.validation.iter().map(|record| {
        WorkflowEvidence::new(
            "validation",
            format!("{}={}", record.command, record.status),
        )
    }));
    evidence
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::model::{ActivityArtifact, ActivitySignal, WorkflowSubject};
    use crate::runtime::validator::{
        DecisionValidator, TransitionAllowlist, TransitionRule, ValidationContext,
        WorkflowDecisionRejectionKind,
    };
    use harness_core::config::workflow::{
        DeclaredState, WorkflowActivityPolicy, WorkflowDefinitionPolicy,
    };
    use serde_json::json;
    use std::collections::BTreeMap;

    fn definition() -> DeclarativeWorkflowDefinition {
        let policy = WorkflowDefinitionPolicy {
            id: "declarative_interpreter_test".to_string(),
            initial: "working".to_string(),
            states: BTreeMap::from([
                (
                    "working".to_string(),
                    DeclaredState {
                        activity: Some("perform_work".to_string()),
                        on_success: Some("done".to_string()),
                        on_signal: BTreeMap::from([
                            ("alpha".to_string(), "done".to_string()),
                            ("beta".to_string(), "failed".to_string()),
                            ("gamma".to_string(), "cancelled".to_string()),
                        ]),
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
            evidence_required: BTreeMap::from([("done".to_string(), vec!["proof".to_string()])]),
            recovery_targets: vec!["working".to_string()],
        };
        super::super::declarative::build_declarative_definition(
            &policy,
            &BTreeMap::from([(
                "perform_work".to_string(),
                WorkflowActivityPolicy::default(),
            )]),
        )
        .expect("test definition should compile")
    }

    fn instance(definition: &DeclarativeWorkflowDefinition, state: &str) -> WorkflowInstance {
        WorkflowInstance::new(
            definition.policy().id.as_str(),
            definition.definition_version(),
            state,
            WorkflowSubject::new("test", "subject"),
        )
        .with_id("workflow-test")
        .with_data(json!({ "definition_hash": definition.definition_hash() }))
    }

    #[test]
    fn mapped_signals_use_lexicographic_precedence_and_project_evidence() -> anyhow::Result<()> {
        let definition = definition();
        let instance = instance(&definition, "working");
        let result = ActivityResult::succeeded("perform_work", "completed")
            .with_signal(ActivitySignal::new("beta", json!({})))
            .with_signal(ActivitySignal::new("alpha", json!({})))
            .with_artifact(ActivityArtifact::new("proof", json!({ "ok": true })));
        let event = WorkflowEvent::new(&instance.id, 1, "RuntimeJobCompleted", "runtime-test");

        let decision = reduce(&definition, &instance, &event, &result)?;

        assert_eq!(decision.next_state, "done");
        assert_eq!(
            decision.commands[0].command_type,
            WorkflowCommandType::MarkDone
        );
        assert!(decision
            .reason
            .contains("lexicographic precedence selected 'alpha'"));
        assert!(decision
            .evidence
            .iter()
            .any(|evidence| evidence.kind == "proof"));
        Ok(())
    }

    #[test]
    fn compiler_attaches_required_evidence_to_every_target_edge() {
        let definition = definition();
        let rule = definition
            .registered()
            .allowlist
            .rule_for("working", "done")
            .expect("done edge should be compiled");
        assert!(rule.required_evidence.contains("proof"));
        assert!(definition
            .registered()
            .allowlist
            .rule_for("__submission__", "working")
            .is_some());
        assert!(definition
            .registered()
            .allowlist
            .rule_for("blocked", "working")
            .is_some());
    }

    #[test]
    fn validator_rejects_missing_generic_required_evidence() {
        let allowlist = TransitionAllowlist::new(vec![TransitionRule::new(
            "checking",
            "passed",
            std::iter::empty(),
        )
        .require_evidence("proof")]);
        let validator = DecisionValidator::new(allowlist);
        let instance = WorkflowInstance::new(
            super::super::quality_gate::QUALITY_GATE_DEFINITION_ID,
            1,
            "checking",
            WorkflowSubject::new("test", "subject"),
        );
        let decision = WorkflowDecision::new(
            &instance.id,
            "checking",
            "quality_passed",
            "passed",
            "completed",
        );
        let rejection = validator
            .validate(
                &instance,
                &decision,
                &ValidationContext::new("runtime-test", chrono::Utc::now()),
            )
            .expect_err("missing evidence should reject the transition");
        assert_eq!(
            rejection.kind,
            WorkflowDecisionRejectionKind::MissingRequiredEvidence
        );
    }

    #[test]
    fn unexpected_activity_blocks_with_operator_attention() -> anyhow::Result<()> {
        let definition = definition();
        let instance = instance(&definition, "working");
        let result = ActivityResult::succeeded("wrong_activity", "completed");
        let event = WorkflowEvent::new(&instance.id, 1, "RuntimeJobCompleted", "runtime-test");

        let decision = reduce(&definition, &instance, &event, &result)?;

        assert_eq!(decision.decision, "block_invalid_agent_output");
        assert_eq!(decision.next_state, "blocked");
        assert_eq!(
            decision
                .commands
                .iter()
                .map(|command| command.command_type)
                .collect::<Vec<_>>(),
            [
                WorkflowCommandType::MarkBlocked,
                WorkflowCommandType::RequestOperatorAttention,
            ]
        );
        Ok(())
    }

    #[test]
    fn submission_and_failure_use_declared_driver_and_terminal_commands() -> anyhow::Result<()> {
        let definition = definition();
        let submission = instance(&definition, "__submission__");
        let decision = build_declarative_submission_decision(&definition, &submission)?;
        assert_eq!(decision.decision, DECLARATIVE_SUBMISSION_DECISION);
        assert_eq!(decision.next_state, "working");
        assert_eq!(
            decision.commands[0].command_type,
            WorkflowCommandType::EnqueueActivity
        );
        assert_eq!(
            decision.commands[0].dedupe_key,
            "workflow-test:working:submit"
        );

        let working = instance(&definition, "working");
        let result = ActivityResult::failed("perform_work", "failed", "boom");
        let event = WorkflowEvent::new(&working.id, 1, "RuntimeJobCompleted", "runtime-test");
        let decision = reduce(&definition, &working, &event, &result)?;
        assert_eq!(decision.next_state, "failed");
        assert_eq!(
            decision.commands[0].command_type,
            WorkflowCommandType::MarkFailed
        );
        Ok(())
    }

    #[test]
    fn missing_definition_validator_accepts_only_fixed_block_contract() -> anyhow::Result<()> {
        let definition = definition();
        let instance = instance(&definition, "working");
        let event = WorkflowEvent::new(&instance.id, 1, "RuntimeJobCompleted", "runtime-test");
        let decision = definition_version_missing_decision(&instance, &event);
        DecisionValidator::definition_version_missing().validate(
            &instance,
            &decision,
            &ValidationContext::new("runtime-test", chrono::Utc::now()),
        )?;
        Ok(())
    }

    #[test]
    fn historical_only_definition_ids_fail_closed_when_the_pin_is_missing() -> anyhow::Result<()> {
        let definition = definition();
        let mut registry =
            super::super::state_registry::WorkflowDefinitionRegistry::new_for_tests();
        registry.register_declarative_historical(definition.clone())?;
        let mut instance = instance(&definition, "working");
        instance.definition_version = instance.definition_version.wrapping_add(1);

        assert!(registry.is_declarative_instance(&instance));
        let validator = registry
            .decision_validator_for_instance(&instance)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "historical declarative id should resolve the missing-version validator"
                )
            })?;
        let event = WorkflowEvent::new(&instance.id, 1, "RuntimeJobCompleted", "runtime-test");
        let decision = definition_version_missing_decision(&instance, &event);
        validator.validate(
            &instance,
            &decision,
            &ValidationContext::new("runtime-test", chrono::Utc::now()),
        )?;
        Ok(())
    }
}
