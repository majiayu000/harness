use super::*;
use crate::runtime::declarative::build_declarative_definition;
use crate::runtime::model::{WorkflowEvidence, WorkflowSubject};
use harness_core::config::workflow::{
    AgentContractMutationPolicy, AgentContractToolPolicy, AgentContractWorkspacePolicy,
    DeclaredProgressMode, DeclaredState, WorkflowActivityPolicy, WorkflowAgentContract,
    WorkflowDefinitionPolicy,
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
        intake: None,
    };
    build_declarative_definition(
        &policy,
        &BTreeMap::from([("run".to_string(), WorkflowActivityPolicy::default())]),
    )
    .expect("fixture definition should compile")
}

fn contract_recovery_definition() -> crate::runtime::declarative::DeclarativeWorkflowDefinition {
    let policy = WorkflowDefinitionPolicy {
        id: "contract_recovery_test".to_string(),
        initial: "blocked".to_string(),
        states: BTreeMap::from([
            (
                "blocked".to_string(),
                DeclaredState {
                    progress: Some(DeclaredProgressMode::OperatorGate),
                    ..DeclaredState::default()
                },
            ),
            (
                "classifying".to_string(),
                DeclaredState {
                    activity: Some("classify".to_string()),
                    on_failure: Some("failed".to_string()),
                    on_signal: BTreeMap::from([
                        ("small".to_string(), "done".to_string()),
                        ("large".to_string(), "failed".to_string()),
                    ]),
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
        recovery_targets: vec!["classifying".to_string()],
        intake: None,
    };
    let contract = WorkflowAgentContract {
        input_schema: "harness.semantic_activity_input.v1".to_string(),
        output_schema: "harness.semantic_verdict.v1".to_string(),
        allowed_outcomes: vec!["small".to_string(), "large".to_string()],
        tools: AgentContractToolPolicy::None,
        mutation: AgentContractMutationPolicy::Forbidden,
        workspace: AgentContractWorkspacePolicy::EphemeralEmpty,
        fresh_context: true,
        max_primary_attempts: 1,
        max_corrections: 1,
    };
    build_declarative_definition(
        &policy,
        &BTreeMap::from([(
            "classify".to_string(),
            WorkflowActivityPolicy {
                prompt: Some("Classify only the pinned input.".to_string()),
                agent_contract: Some(contract),
                ..WorkflowActivityPolicy::default()
            },
        )]),
    )
    .expect("contract recovery definition should compile")
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
fn automatic_unblock_preserves_feedback_repair_budget() {
    let definition = definition();
    let mut instance = instance(&definition).with_server_data(serde_json::json!({
        "feedback_repair_round": 3,
        "feedback_repair_blocker_count": 1,
        "feedback_repair_lane": "remote_feedback",
    }));

    persist_operator_recovery_data(
        &mut instance,
        WorkflowRuntimeRecoveryAction::Unblock,
        "transient stop recheck",
        "auto_recovery",
        "blocked",
        "running",
        "event-one",
    )
    .expect("automatic recovery metadata should persist");

    assert_eq!(instance.data["feedback_repair_round"], 3);
    assert_eq!(instance.data["feedback_repair_blocker_count"], 1);
    assert_eq!(instance.data["feedback_repair_lane"], "remote_feedback");
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
        let plan =
            declarative_recovery_dispatch_plan(&request(Some(target), &[]), &definition, &instance)
                .expect("plan should build")
                .expect("target should have a driver");
        assert!(matches!(
            &plan.command_source,
            RecoveryDispatchCommandSource::DeclarativeProgress(command) if command.command_type == expected
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
    let plan = declarative_recovery_dispatch_plan(
        &request(Some("running"), &evidence),
        &definition,
        &instance,
    )
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

#[test]
fn declarative_recovery_rebuilds_the_complete_pinned_contract_input() {
    let definition = contract_recovery_definition();
    let instance = WorkflowInstance::new(
        definition.policy().id.clone(),
        definition.definition_version(),
        "blocked",
        WorkflowSubject::new("issue", "owner/repo#42"),
    )
    .with_server_data(serde_json::json!({
        "definition_hash": definition.definition_hash(),
        "changed_files": ["src/lib.rs"],
    }));
    let plan = declarative_recovery_dispatch_plan(
        &request(Some("classifying"), &[]),
        &definition,
        &instance,
    )
    .expect("plan should build")
    .expect("contract target should have a driver");
    let RecoveryDispatchCommandSource::DeclarativeProgress(command) = &plan.command_source else {
        panic!("contract recovery must produce a declarative command")
    };
    let input = &command.command["agent_contract_input"];
    assert_eq!(input["subject"]["kind"], "issue");
    assert_eq!(input["subject"]["identity"], "owner/repo#42");
    assert_eq!(input["facts"], instance.data);
    assert_eq!(
        input["provenance"],
        serde_json::to_value(
            instance
                .data_provenance
                .as_ref()
                .expect("server facts have provenance")
        )
        .expect("serialize provenance")
    );
    assert_eq!(command.command["prompt"], "Classify only the pinned input.");
    assert_eq!(
        command.command["definition_hash"],
        definition.definition_hash()
    );
}
