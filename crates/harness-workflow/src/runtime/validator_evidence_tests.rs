use super::*;
use crate::runtime::declarative::workflow_evidence_from_activity_artifacts;
use crate::runtime::model::{
    ActivityArtifact, WorkflowCommand, WorkflowCommandType, WorkflowEvidence,
};
use crate::runtime::validator::{DecisionValidator, TransitionAllowlist, TransitionRule};
use serde_json::json;

fn rule(operator_recovery_only: bool) -> TransitionRule {
    let mut rule =
        TransitionRule::new("blocked", "running", [WorkflowCommandType::EnqueueActivity]);
    rule.required_evidence = ["ReviewReport".to_string(), "tests".to_string()].into();
    rule.operator_recovery_only = operator_recovery_only;
    rule
}

fn decision(name: &str) -> WorkflowDecision {
    WorkflowDecision::new("one", "blocked", name, "running", "test")
        .with_command(WorkflowCommand::enqueue_activity("run", "run:one"))
}

#[test]
fn required_evidence_is_complete_and_case_sensitive() {
    let context = ValidationContext::new("workflow_runtime_operator_action", chrono::Utc::now());
    let missing =
        validate_declarative_transition_metadata(&rule(false), &decision("advance"), &context)
            .expect_err("missing evidence must fail");
    assert_eq!(
        missing.kind,
        WorkflowDecisionRejectionKind::MissingRequiredEvidence
    );
    let wrong_case = decision("advance")
        .with_evidence(WorkflowEvidence::new("reviewreport", "wrong case"))
        .with_evidence(WorkflowEvidence::new("tests", "ok"));
    assert!(validate_declarative_transition_metadata(&rule(false), &wrong_case, &context).is_err());
    let complete = decision("advance")
        .with_evidence(WorkflowEvidence::new("ReviewReport", "ok"))
        .with_evidence(WorkflowEvidence::new("tests", "ok"));
    validate_declarative_transition_metadata(&rule(false), &complete, &context)
        .expect("all exact evidence kinds should pass");
}

#[test]
fn retry_exemption_is_narrow_and_operator_recovery_is_authorized_exactly() {
    let runtime = ValidationContext::new("runtime", chrono::Utc::now());
    let mut retry_rule = rule(false);
    retry_rule.from_state = Some("running".to_string());
    let retry = WorkflowDecision::new(
        "one",
        "running",
        "retry_failed_runtime_activity",
        "running",
        "retry",
    )
    .with_command(WorkflowCommand::enqueue_activity("run", "retry:one"));
    validate_declarative_transition_metadata(&retry_rule, &retry, &runtime)
        .expect("exact activity retry may omit completion evidence");
    let ordinary = WorkflowDecision {
        decision: "advance".to_string(),
        ..retry
    };
    assert!(validate_declarative_transition_metadata(&retry_rule, &ordinary, &runtime).is_err());

    let operator = ValidationContext::new("workflow_runtime_operator_action", chrono::Utc::now());
    let recovery = decision("operator_runtime_unblock")
        .with_evidence(WorkflowEvidence::new("ReviewReport", "ok"))
        .with_evidence(WorkflowEvidence::new("tests", "ok"));
    validate_declarative_transition_metadata(&rule(true), &recovery, &operator)
        .expect("exact operator recovery should pass");
    assert!(validate_declarative_transition_metadata(&rule(true), &recovery, &runtime).is_err());
    let wrong_decision = WorkflowDecision {
        decision: "advance".to_string(),
        ..recovery
    };
    assert!(
        validate_declarative_transition_metadata(&rule(true), &wrong_decision, &operator).is_err()
    );
}

#[test]
fn only_nonempty_nondecision_artifact_types_become_evidence() {
    let artifacts = [
        ActivityArtifact::new("ReviewReport", json!({"ok": true})),
        ActivityArtifact::new("workflow_decision", json!({"decision": "forged"})),
    ];
    let evidence =
        workflow_evidence_from_activity_artifacts(&artifacts).expect("valid artifacts should map");
    assert_eq!(
        evidence
            .iter()
            .map(|item| item.kind.as_str())
            .collect::<Vec<_>>(),
        ["ReviewReport"]
    );
    assert!(
        workflow_evidence_from_activity_artifacts(&[ActivityArtifact::new(" ", json!({}))])
            .is_err()
    );
}

#[test]
fn transition_allowlist_keeps_rule_metadata() {
    let mut metadata_rule = rule(true);
    metadata_rule.required_command = Some(WorkflowCommandType::EnqueueActivity);
    let validator = DecisionValidator::new(TransitionAllowlist::new(vec![metadata_rule]));
    let compiled = validator
        .transition_rules_from("blocked")
        .next()
        .expect("rule");
    assert!(compiled.operator_recovery_only);
    assert_eq!(
        compiled.required_command,
        Some(WorkflowCommandType::EnqueueActivity)
    );
    assert_eq!(compiled.required_evidence.len(), 2);
}

#[test]
fn declarative_rule_rejects_a_missing_target_driver() {
    let mut driver_rule = rule(false);
    driver_rule.required_command = Some(WorkflowCommandType::EnqueueActivity);
    let decision = WorkflowDecision::new("one", "blocked", "advance", "running", "test")
        .with_evidence(WorkflowEvidence::new("ReviewReport", "ok"))
        .with_evidence(WorkflowEvidence::new("tests", "ok"));
    let rejection = validate_declarative_transition_metadata(
        &driver_rule,
        &decision,
        &ValidationContext::new("runtime", chrono::Utc::now()),
    )
    .expect_err("declarative target driver must be present");
    assert_eq!(
        rejection.kind,
        WorkflowDecisionRejectionKind::RequiredCommandMissing
    );
}
