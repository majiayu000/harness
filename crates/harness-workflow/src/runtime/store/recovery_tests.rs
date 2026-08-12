use super::*;
use crate::runtime::declarative::build_declarative_definition;
use crate::runtime::model::{WorkflowEvidence, WorkflowSubject};
use harness_core::config::workflow::{
    DeclaredProgressMode, DeclaredState, WorkflowActivityPolicy, WorkflowDefinitionPolicy,
};
use serde_json::json;
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

#[test]
fn dependency_gate_recovery_builds_override_plan_and_evidence() {
    for (force_execute, expected_state, expected_activity) in [
        (false, "planning", "plan_issue"),
        (true, "implementing", "implement_issue"),
    ] {
        let mut instance = WorkflowInstance::new(
            GITHUB_ISSUE_PR_DEFINITION_ID,
            1,
            "awaiting_dependencies",
            WorkflowSubject::new("issue", "issue:1885"),
        )
        .with_id(format!("dependency-override-{force_execute}"))
        .with_server_data(json!({
            "project_id": "/project-a",
            "repo": "owner/repo",
            "issue_number": 1885,
            "task_id": "github-issue:owner/repo:issue:1885",
            "source": "github",
            "external_id": "1885",
            "depends_on": ["github-issue:owner/repo:issue:1884"],
            "dependencies_blocked": true,
            "force_execute": force_execute,
            "last_remote_fact_hash": "sha256:abc",
        }));
        let request = WorkflowRuntimeRecoveryRequest {
            workflow_id: "dependency-override",
            action: WorkflowRuntimeRecoveryAction::Unblock,
            reason: "operator approved dependency override",
            actor: "operator",
            target_state: None,
            evidence: &[],
        };

        assert!(recovery_rejection(&instance, &request)
            .expect("rejection check should parse")
            .is_none());

        let plan = awaiting_dependencies_recovery_dispatch_plan(&instance, &request);
        assert_eq!(plan.target.state, expected_state);
        assert_eq!(plan.target.activity.as_deref(), Some(expected_activity));
        let command = recovery_dispatch_command(
            &instance,
            WorkflowRuntimeRecoveryAction::Unblock,
            request.reason,
            &plan,
            "event-one",
        );
        assert_eq!(command.command_type, WorkflowCommandType::EnqueueActivity);
        assert_eq!(command.activity_name(), Some(expected_activity));
        assert_eq!(command.command["issue_number"], 1885);
        assert_eq!(
            command.command["dispatch_gate"]["reason"],
            "operator_dependency_override"
        );
        assert_eq!(command.command["dispatch_gate"]["fact_hash"], "sha256:abc");
        assert!(command.command["additional_prompt"]
            .as_str()
            .is_some_and(|prompt| prompt.contains("overriding the dependency gate")));

        persist_operator_recovery_data(
            &mut instance,
            WorkflowRuntimeRecoveryAction::Unblock,
            request.reason,
            request.actor,
            "awaiting_dependencies",
            expected_state,
            "event-one",
        )
        .expect("operator recovery data should persist");
        assert_eq!(instance.data["dependencies_blocked"], false);
        assert_eq!(instance.data["dependency_override"]["action"], "unblock");
        assert_eq!(
            instance.data["dependency_override"]["previous_state"],
            "awaiting_dependencies"
        );
        assert_eq!(
            instance.data["dependency_override"]["state"],
            expected_state
        );
        assert_eq!(
            instance.data["dependency_override"]["event_id"],
            "event-one"
        );
        assert_eq!(
            instance.data["last_operator_recovery"]["previous_state"],
            "awaiting_dependencies"
        );
    }
}
