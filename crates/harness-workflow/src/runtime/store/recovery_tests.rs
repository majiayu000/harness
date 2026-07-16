use super::*;
use crate::runtime::declarative::build_declarative_definition;
use crate::runtime::model::{WorkflowEvidence, WorkflowSubject};
use harness_core::config::workflow::{
    DeclaredProgressMode, DeclaredState, WorkflowActivityPolicy, WorkflowDefinitionPolicy,
};
use std::collections::BTreeMap;

fn definition() -> crate::runtime::declarative::DeclarativeWorkflowDefinition {
    let policy = WorkflowDefinitionPolicy {
        id: "recovery_test".to_string(),
        initial: "running".to_string(),
        states: BTreeMap::from([
            (
                "blocked".to_string(),
                DeclaredState {
                    progress: Some(DeclaredProgressMode::OperatorGate),
                    ..DeclaredState::default()
                },
            ),
            (
                "running".to_string(),
                DeclaredState {
                    activity: Some("run".to_string()),
                    on_success: Some("done".to_string()),
                    on_failure: Some("failed".to_string()),
                    on_signal: BTreeMap::from([("cancel".to_string(), "cancelled".to_string())]),
                    ..DeclaredState::default()
                },
            ),
            (
                "waiting".to_string(),
                DeclaredState {
                    progress: Some(DeclaredProgressMode::ExternalWait),
                    ..DeclaredState::default()
                },
            ),
            (
                "approval".to_string(),
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
        evidence_required: BTreeMap::from([(
            "running".to_string(),
            vec!["operator_ticket".to_string()],
        )]),
        recovery_targets: vec![
            "running".to_string(),
            "waiting".to_string(),
            "approval".to_string(),
        ],
    };
    build_declarative_definition(
        &policy,
        &BTreeMap::from([("run".to_string(), WorkflowActivityPolicy::default())]),
    )
    .expect("fixture definition should compile")
}

fn instance(
    definition: &crate::runtime::declarative::DeclarativeWorkflowDefinition,
) -> WorkflowInstance {
    WorkflowInstance::new(
        "recovery_test",
        definition.definition_version(),
        "blocked",
        WorkflowSubject::new("test", "one"),
    )
}

fn request<'a>(
    target_state: Option<&'a str>,
    evidence: &'a [WorkflowEvidence],
) -> WorkflowRuntimeRecoveryRequest<'a> {
    WorkflowRuntimeRecoveryRequest {
        workflow_id: "one",
        action: WorkflowRuntimeRecoveryAction::Unblock,
        reason: "fixed",
        actor: "operator",
        target_state,
        evidence,
    }
}

#[test]
fn declarative_recovery_requires_operator_and_pinned_target_selection() {
    let definition = definition();
    let instance = instance(&definition);
    let mut automatic = request(Some("running"), &[]);
    automatic.actor = "auto_recovery";
    assert!(matches!(
        declarative_recovery_rejection(&instance, &automatic, &definition),
        Some(WorkflowRuntimeRecoveryOutcome::OperatorRequired { .. })
    ));
    assert!(matches!(
        declarative_recovery_rejection(&instance, &request(None, &[]), &definition),
        Some(WorkflowRuntimeRecoveryOutcome::TargetRequired { .. })
    ));
    assert!(matches!(
        declarative_recovery_rejection(&instance, &request(Some("other"), &[]), &definition),
        Some(WorkflowRuntimeRecoveryOutcome::TargetNotAllowed { .. })
    ));
    assert!(
        declarative_recovery_rejection(&instance, &request(Some("running"), &[]), &definition)
            .is_none()
    );
}

#[test]
fn declarative_recovery_builds_exact_progress_driver_and_preserves_evidence() {
    let definition = definition();
    let instance = instance(&definition);
    for (target, expected) in [
        ("running", WorkflowCommandType::EnqueueActivity),
        ("waiting", WorkflowCommandType::Wait),
        ("approval", WorkflowCommandType::RequestOperatorAttention),
    ] {
        let plan = declarative_recovery_dispatch_plan(&request(Some(target), &[]), &definition)
            .expect("plan should build")
            .expect("target should have a driver");
        assert!(matches!(
            plan.command_source,
            RecoveryDispatchCommandSource::DeclarativeProgress(command_type) if command_type == expected
        ));
        let command = recovery_dispatch_command(
            &instance,
            WorkflowRuntimeRecoveryAction::Unblock,
            "fixed",
            &plan,
            "event-one",
        );
        assert_eq!(
            recovery_command_status(&command),
            if expected == WorkflowCommandType::EnqueueActivity {
                WorkflowCommandStatus::Pending
            } else {
                WorkflowCommandStatus::HandledInline
            }
        );
    }
    let evidence = [WorkflowEvidence::new("operator_ticket", "approved")];
    let plan =
        declarative_recovery_dispatch_plan(&request(Some("running"), &evidence), &definition)
            .expect("plan should build")
            .expect("target should have a driver");
    let decision = recovery_dispatch_decision(
        &instance,
        WorkflowRuntimeRecoveryAction::Unblock,
        "fixed",
        "blocked",
        &plan,
        "event-one",
        &evidence,
    );
    assert_eq!(decision.evidence, evidence);
    assert_eq!(
        decision.commands[0].command_type,
        WorkflowCommandType::EnqueueActivity
    );
    assert_eq!(decision.commands[0].activity_name(), Some("run"));
}
