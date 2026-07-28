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

/// GH-1766 Evidence Contract table test: every contracted transition of the
/// built-in definitions requires exactly the named evidence class, and every
/// other explicit rule keeps an empty requirement set.
#[test]
fn builtin_evidence_contract_matches_spec_table() {
    use crate::runtime::completion_evidence::{
        EVIDENCE_GITHUB_TERMINAL, EVIDENCE_PROMPT_COMPLETION, EVIDENCE_SERVER_PR_SNAPSHOT,
        EVIDENCE_SERVER_VALIDATION_DIGEST, EVIDENCE_VERIFIED_PR_BINDING,
    };

    /// definition id, its allowlist, and the contracted
    /// `(from, to, required evidence kinds)` rows from `product.md`.
    type EvidenceContract = (&'static str, TransitionAllowlist, Vec<ContractRow>);
    type ContractRow = (&'static str, &'static str, Vec<&'static str>);

    let contract: &[EvidenceContract] = &[
        (
            "github_issue_pr",
            TransitionAllowlist::github_issue_pr_defaults(),
            vec![
                (
                    "implementing",
                    "pr_open",
                    vec![EVIDENCE_VERIFIED_PR_BINDING],
                ),
                ("implementing", "done", vec![EVIDENCE_GITHUB_TERMINAL]),
                ("pr_open", "done", vec![EVIDENCE_GITHUB_TERMINAL]),
                ("awaiting_feedback", "done", vec![EVIDENCE_GITHUB_TERMINAL]),
                (
                    "addressing_feedback",
                    "done",
                    vec![EVIDENCE_GITHUB_TERMINAL],
                ),
                (
                    "quality_gate_pending",
                    "done",
                    vec![EVIDENCE_GITHUB_TERMINAL],
                ),
                ("ready_to_merge", "done", vec![EVIDENCE_GITHUB_TERMINAL]),
                ("merging", "done", vec![EVIDENCE_GITHUB_TERMINAL]),
            ],
        ),
        (
            "quality_gate",
            TransitionAllowlist::quality_gate_defaults(),
            vec![(
                "checking",
                "passed",
                vec![EVIDENCE_SERVER_VALIDATION_DIGEST],
            )],
        ),
        (
            "pr_feedback",
            TransitionAllowlist::pr_feedback_defaults(),
            vec![(
                "inspecting",
                "ready_to_merge",
                vec![EVIDENCE_SERVER_PR_SNAPSHOT],
            )],
        ),
        (
            "prompt_task",
            TransitionAllowlist::prompt_task_defaults(),
            vec![("implementing", "done", vec![EVIDENCE_PROMPT_COMPLETION])],
        ),
    ];

    for (definition, allowlist, required) in contract {
        for (from, to, evidence) in required {
            let rule = allowlist
                .rule_for(from, to)
                .unwrap_or_else(|| panic!("{definition}: missing rule {from} -> {to}"));
            for kind in evidence {
                assert!(
                    rule.required_evidence.contains(*kind),
                    "{definition}: {from} -> {to} must require `{kind}`"
                );
            }
        }
        for rule in allowlist.rules() {
            let Some(from) = rule.from_state.as_deref() else {
                assert!(
                    rule.required_evidence.is_empty(),
                    "{definition}: from_any rules must not carry evidence requirements"
                );
                continue;
            };
            let contracted = required.iter().any(|(contract_from, contract_to, _)| {
                *contract_from == from && *contract_to == rule.to_state
            });
            assert_eq!(
                !rule.required_evidence.is_empty(),
                contracted,
                "{definition}: {from} -> {} evidence requirement diverges from the contract table",
                rule.to_state
            );
        }
    }
}

/// GH-1766: a contracted transition rejects a decision missing its evidence
/// class with the typed reason, and accepts the same decision carrying it.
#[test]
fn contracted_transition_rejects_without_evidence_and_accepts_with_it() {
    use crate::runtime::completion_evidence::EVIDENCE_SERVER_VALIDATION_DIGEST;

    let allowlist = TransitionAllowlist::quality_gate_defaults();
    let Some(rule) = allowlist.rule_for("checking", "passed") else {
        panic!("quality_gate defaults must declare checking -> passed");
    };
    let context = ValidationContext::new("runtime-worker", chrono::Utc::now());
    let bare = WorkflowDecision::new("wf-1", "checking", "quality_passed", "passed", "test");
    let rejection = validate_declarative_transition_metadata(rule, &bare, &context)
        .expect_err("missing digest evidence must reject");
    assert_eq!(
        rejection.kind,
        WorkflowDecisionRejectionKind::MissingRequiredEvidence
    );
    assert!(rejection
        .message
        .contains(EVIDENCE_SERVER_VALIDATION_DIGEST));

    let evidenced = bare.with_evidence(WorkflowEvidence::new(
        EVIDENCE_SERVER_VALIDATION_DIGEST,
        "server executed 2 validation command(s), all exit 0",
    ));
    if let Err(rejection) = validate_declarative_transition_metadata(rule, &evidenced, &context) {
        panic!("digest evidence should satisfy the contract: {rejection}");
    }
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
