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
            "additional_prompt": "Preserve the operator's issue-specific instruction.",
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

        let plan = dependency_override_recovery::dispatch_plan(&instance, &request)
            .expect("dependency override plan should build");
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
        assert!(command.command["additional_prompt"]
            .as_str()
            .is_some_and(|prompt| prompt.contains("issue-specific instruction")));

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

#[test]
fn dependency_cycle_retry_builds_override_plan_and_cleans_failure_marker() {
    for (force_execute, expected_state, expected_activity) in [
        (false, "planning", "plan_issue"),
        (true, "implementing", "implement_issue"),
    ] {
        let mut instance = WorkflowInstance::new(
            GITHUB_ISSUE_PR_DEFINITION_ID,
            1,
            "failed",
            WorkflowSubject::new("issue", "issue:1885"),
        )
        .with_id(format!("dependency-cycle-retry-{force_execute}"))
        .with_server_data(json!({
            "project_id": "/project-a",
            "issue_number": 1885,
            "dependencies_blocked": true,
            "dependency_failure_status": "dependency_cycle",
            "force_execute": force_execute,
        }));
        let request = WorkflowRuntimeRecoveryRequest {
            workflow_id: "dependency-cycle-retry",
            action: WorkflowRuntimeRecoveryAction::Retry,
            reason: "operator approved dependency override",
            actor: "operator",
            target_state: None,
            evidence: &[],
        };

        assert!(recovery_rejection(&instance, &request)
            .expect("rejection check should parse")
            .is_none());
        assert!(dependency_override_recovery::matches(
            &instance,
            request.action
        ));
        let plan = dependency_override_recovery::dispatch_plan(&instance, &request)
            .expect("cycle retry plan should build");
        assert_eq!(plan.target.state, expected_state);
        assert_eq!(plan.target.activity.as_deref(), Some(expected_activity));
        let decision = recovery_dispatch_decision(
            &instance,
            request.action,
            request.reason,
            "failed",
            &plan,
            "event-one",
            &[],
        );
        assert_eq!(decision.decision, "operator_runtime_retry");
        assert!(decision.commands[0]
            .dedupe_key
            .starts_with("operator-recovery:retry:"));
        validator_for_instance(&instance)
            .expect("validator lookup should succeed")
            .expect("GitHub issue workflow should have a validator")
            .validate(
                &instance,
                &decision,
                &ValidationContext::new("workflow_runtime_operator_action", chrono::Utc::now())
                    .allow_terminal_reopen(),
            )
            .expect("cycle retry should be a valid terminal reopen");

        persist_operator_recovery_data(
            &mut instance,
            request.action,
            request.reason,
            request.actor,
            "failed",
            expected_state,
            "event-one",
        )
        .expect("operator recovery data should persist");
        assert_eq!(instance.data["dependencies_blocked"], false);
        assert_eq!(instance.data["dependency_override"]["action"], "retry");
        assert_eq!(
            instance.data["dependency_override"]["previous_state"],
            "failed"
        );
        assert_eq!(instance.data["last_operator_recovery"]["action"], "retry");
        assert_eq!(
            instance.data["last_operator_recovery"]["previous_state"],
            "failed"
        );
        assert!(instance.data.get("dependency_failure_status").is_none());
    }

    let malformed = WorkflowInstance::new(
        GITHUB_ISSUE_PR_DEFINITION_ID,
        1,
        "failed",
        WorkflowSubject::new("issue", "issue:1886"),
    )
    .with_server_data(json!({
        "dependency_failure_status": "dependency_cycle",
        "candidate_fanout": {
            "candidate_group_id": "dependency-cycle-malformed",
            "candidate_count": "two",
        },
    }));
    let request = WorkflowRuntimeRecoveryRequest {
        workflow_id: "dependency-cycle-malformed",
        action: WorkflowRuntimeRecoveryAction::Retry,
        reason: "operator approved dependency override",
        actor: "operator",
        target_state: None,
        evidence: &[],
    };
    let error = dependency_override_recovery::dispatch_plan(&malformed, &request)
        .expect_err("malformed cycle fan-out must fail before recovery mutation");
    assert!(error.to_string().contains("candidate_fanout"));
}

#[test]
fn dependency_gate_recovery_preserves_candidate_fanout_for_force_execute() -> anyhow::Result<()> {
    let mut instance = WorkflowInstance::new(
        GITHUB_ISSUE_PR_DEFINITION_ID,
        1,
        "awaiting_dependencies",
        WorkflowSubject::new("issue", "issue:1885"),
    )
    .with_id("dependency-override-fanout")
    .with_server_data(json!({
        "project_id": "/project-a",
        "repo": "owner/repo",
        "issue_number": 1885,
        "task_id": "github-issue:owner/repo:issue:1885",
        "source": "github",
        "external_id": "1885",
        "depends_on": ["github-issue:owner/repo:issue:1884"],
        "dependencies_blocked": true,
        "force_execute": true,
        "candidate_fanout": {
            "candidate_group_id": "dependency-override-fanout:candidate-group:issue-1885",
            "candidate_count": 2,
            "trigger_label": "best-of-n",
            "max_turns_per_candidate": 4,
        },
        "last_remote_fact_hash": "sha256:abc",
    }));
    let request = WorkflowRuntimeRecoveryRequest {
        workflow_id: "dependency-override-fanout",
        action: WorkflowRuntimeRecoveryAction::Unblock,
        reason: "operator approved dependency override",
        actor: "operator",
        target_state: None,
        evidence: &[],
    };

    let plan = dependency_override_recovery::dispatch_plan(&instance, &request)
        .expect("dependency override plan should build");
    let decision = recovery_dispatch_decision(
        &instance,
        WorkflowRuntimeRecoveryAction::Unblock,
        request.reason,
        "awaiting_dependencies",
        &plan,
        "event-one",
        &[],
    );

    assert_eq!(decision.next_state, "implementing");
    assert_eq!(decision.commands.len(), 2);
    for (candidate_index, command) in (1..=2).zip(&decision.commands) {
        assert_eq!(command.activity_name(), Some("implement_issue"));
        assert_eq!(command.command["submission_mode"], "deferred");
        assert_eq!(
            command.command["candidate"]["candidate_index"],
            candidate_index
        );
        assert_eq!(command.command["candidate"]["candidate_count"], 2);
        assert_eq!(
            command.command["candidate"]["budget"]["max_turns_per_candidate"],
            4
        );
        assert_eq!(
            command.dedupe_key,
            format!(
                "operator-recovery:unblock:dependency-override-fanout:event-one:candidate:c{candidate_index}"
            )
        );
    }

    validator_for_instance(&instance)?
        .expect("GitHub issue workflow should have a validator")
        .validate(
            &instance,
            &decision,
            &ValidationContext::new("workflow_runtime_operator_action", chrono::Utc::now()),
        )?;

    persist_operator_recovery_data(
        &mut instance,
        WorkflowRuntimeRecoveryAction::Unblock,
        request.reason,
        request.actor,
        "awaiting_dependencies",
        "implementing",
        "event-one",
    )
    .expect("operator recovery data should persist");
    assert_eq!(instance.data["dependencies_blocked"], false);
    Ok(())
}

#[test]
fn dependency_gate_recovery_validates_candidate_fanout_before_planning() {
    let instance = WorkflowInstance::new(
        GITHUB_ISSUE_PR_DEFINITION_ID,
        1,
        "awaiting_dependencies",
        WorkflowSubject::new("issue", "issue:1885"),
    )
    .with_id("dependency-override-invalid-fanout")
    .with_server_data(json!({
        "project_id": "/project-a",
        "issue_number": 1885,
        "dependencies_blocked": true,
        "force_execute": false,
        "candidate_fanout": {
            "candidate_group_id": "dependency-override-invalid-fanout:candidate-group:issue-1885",
            "candidate_count": "two",
        },
    }));
    let request = WorkflowRuntimeRecoveryRequest {
        workflow_id: "dependency-override-invalid-fanout",
        action: WorkflowRuntimeRecoveryAction::Unblock,
        reason: "operator approved dependency override",
        actor: "operator",
        target_state: None,
        evidence: &[],
    };

    let error = dependency_override_recovery::dispatch_plan(&instance, &request)
        .expect_err("malformed candidate fan-out must fail before planning");

    assert!(error.to_string().contains("candidate_fanout"));
}

#[test]
fn dependency_gate_recovery_validates_but_defers_fanout_until_after_planning() {
    let mut instance = WorkflowInstance::new(
        GITHUB_ISSUE_PR_DEFINITION_ID,
        1,
        "awaiting_dependencies",
        WorkflowSubject::new("issue", "issue:1885"),
    )
    .with_id("dependency-override-deferred-fanout")
    .with_server_data(json!({
        "project_id": "/project-a",
        "issue_number": 1885,
        "force_execute": false,
        "candidate_fanout": {
            "candidate_group_id": "dependency-override-deferred-fanout:candidate-group:issue-1885",
            "candidate_count": 2,
            "trigger_label": "best-of-n",
        },
    }));
    let request = WorkflowRuntimeRecoveryRequest {
        workflow_id: "dependency-override-deferred-fanout",
        action: WorkflowRuntimeRecoveryAction::Unblock,
        reason: "operator approved dependency override",
        actor: "operator",
        target_state: None,
        evidence: &[],
    };

    let plan = dependency_override_recovery::dispatch_plan(&instance, &request)
        .expect("valid fan-out metadata should allow recovery planning");
    let decision = recovery_dispatch_decision(
        &instance,
        request.action,
        request.reason,
        "awaiting_dependencies",
        &plan,
        "event-one",
        &[],
    );

    assert_eq!(decision.next_state, "planning");
    assert_eq!(decision.commands.len(), 1);
    assert_eq!(decision.commands[0].activity_name(), Some("plan_issue"));
    assert!(decision.commands[0].command.get("candidate").is_none());
    instance.state = "planning".to_string();
    persist_operator_recovery_data(
        &mut instance,
        request.action,
        request.reason,
        request.actor,
        "awaiting_dependencies",
        "planning",
        "event-one",
    )
    .expect("recovery metadata should persist");
    assert!(instance.data.get("candidate_fanout").is_some());

    let result =
        crate::runtime::model::ActivityResult::succeeded("plan_issue", "Issue plan ready.")
            .with_artifact(crate::runtime::model::ActivityArtifact::new(
                "issue_plan",
                json!({
                    "summary": "Implement the dependency-gated issue.",
                    "task_class": "runtime_or_data",
                    "target_files": ["crates/harness-workflow/src/runtime/store/recovery.rs"],
                    "validation_plan": ["cargo test -p harness-workflow dependency_gate_recovery"],
                    "blockers": [],
                }),
            ));
    let event = crate::runtime::model::WorkflowEvent::new(
        &instance.id,
        1,
        crate::runtime::RUNTIME_JOB_COMPLETED_EVENT,
        "runtime-one",
    )
    .with_payload(json!({
        "command_id": "plan-command",
        "command": decision.commands[0],
        "runtime_job_id": "runtime-one",
        "activity_result": result,
    }));
    let implementation = crate::runtime::reduce_runtime_job_completed(&instance, &event)
        .expect("plan completion event should parse")
        .expect("plan completion should start implementation");
    assert_eq!(implementation.commands.len(), 2);
    for (candidate_index, command) in (1..=2).zip(&implementation.commands) {
        assert_eq!(command.activity_name(), Some("implement_issue"));
        assert_eq!(command.command["submission_mode"], "deferred");
        assert_eq!(
            command.command["candidate"]["candidate_index"],
            candidate_index
        );
    }
}
