use crate::stack_policy::{
    StackChangeFact, StackEvidenceCompleteness, StackEvidenceKind, StackPolicyDecision,
    StackPolicyDocument, StackPolicyEngine, StackPolicyError, StackPolicyFacts,
    STACK_POLICY_FACTS_SCHEMA_VERSION,
};
use harness_core::stack::capability_evidence::AgentStackCapabilityEvidenceClass;
use harness_core::stack::{
    AgentStackCapability, AgentStackProtectionConfidence, AgentStackProtectionDiffKind,
    AgentStackProtectionRole, AgentStackTrustLevel,
};

fn complete_evidence() -> StackEvidenceCompleteness {
    StackEvidenceCompleteness::complete([
        StackEvidenceKind::StructuralDiff,
        StackEvidenceKind::CapabilityEvidence,
        StackEvidenceKind::ObservedCapabilityUse,
        StackEvidenceKind::ProtectiveControlDiff,
        StackEvidenceKind::ValidationStatus,
    ])
}

fn facts(facts: impl IntoIterator<Item = StackChangeFact>) -> StackPolicyFacts {
    StackPolicyFacts::new(complete_evidence(), facts).expect("facts should validate")
}

fn reference_eval(facts: StackPolicyFacts) -> crate::stack_policy::StackPolicyEvaluation {
    StackPolicyEngine::conservative_reference()
        .evaluate(&facts)
        .expect("reference policy should evaluate")
}

#[test]
fn stack_policy_promotes_when_complete_evidence_has_no_risk_facts() {
    let evaluation = reference_eval(facts([StackChangeFact::ValidationStatus {
        fact_id: "validation-pass".to_string(),
        check_id: "cargo-test".to_string(),
        passed: true,
    }]));

    assert_eq!(evaluation.decision, StackPolicyDecision::Promote);
    assert_eq!(
        evaluation.winning_rule_ids,
        vec!["ASC-012-PROMOTE-COMPLETE-EVIDENCE-NO-RISK"]
    );
    assert!(!evaluation.conflicted);
    assert!(evaluation
        .matched_rules
        .iter()
        .all(|rule_match| rule_match.reason.contains("evidence")
            || rule_match.reason.contains("risk")));
}

#[test]
fn stack_policy_reviews_sensitive_new_authority_with_matched_fact() {
    let evaluation = reference_eval(facts([StackChangeFact::CapabilityEvidence {
        fact_id: "cap-network-declared".to_string(),
        component_id: "repository:policy:WORKFLOW.md".to_string(),
        evidence_class: AgentStackCapabilityEvidenceClass::Declared,
        capability: AgentStackCapability::Network,
        trust_level: AgentStackTrustLevel::RepositoryObserved,
    }]));

    assert_eq!(evaluation.decision, StackPolicyDecision::Review);
    assert_eq!(
        evaluation.winning_rule_ids,
        vec!["ASC-012-REVIEW-SENSITIVE-AUTHORITY-DECLARED"]
    );
    assert!(!evaluation.conflicted);
    let review_match = evaluation
        .matched_rules
        .iter()
        .find(|rule_match| rule_match.rule_id == "ASC-012-REVIEW-SENSITIVE-AUTHORITY-DECLARED")
        .expect("review rule should match");
    assert_eq!(review_match.matched_facts, vec!["cap-network-declared"]);
}

#[test]
fn stack_policy_blocks_protective_control_weakening() {
    let evaluation = reference_eval(facts([StackChangeFact::ProtectiveControlDiff {
        fact_id: "control-removed".to_string(),
        diff_kind: AgentStackProtectionDiffKind::Removed,
        roles: vec![AgentStackProtectionRole::Policy],
        confidence: AgentStackProtectionConfidence::High,
    }]));

    assert_eq!(evaluation.decision, StackPolicyDecision::Block);
    assert_eq!(
        evaluation.winning_rule_ids,
        vec!["ASC-012-BLOCK-PROTECTIVE-CONTROL-WEAKENED"]
    );
    assert!(!evaluation.conflicted);
    let block_match = evaluation
        .matched_rules
        .iter()
        .find(|rule_match| rule_match.rule_id == "ASC-012-BLOCK-PROTECTIVE-CONTROL-WEAKENED")
        .expect("block rule should match");
    assert_eq!(block_match.matched_facts, vec!["control-removed"]);
    assert_eq!(
        block_match.precedence,
        StackPolicyDecision::Block.precedence()
    );
}

#[test]
fn stack_policy_reports_conflict_and_uses_block_precedence() {
    let evaluation = reference_eval(facts([
        StackChangeFact::CapabilityEvidence {
            fact_id: "cap-network-declared".to_string(),
            component_id: "repository:policy:WORKFLOW.md".to_string(),
            evidence_class: AgentStackCapabilityEvidenceClass::Declared,
            capability: AgentStackCapability::Network,
            trust_level: AgentStackTrustLevel::RepositoryObserved,
        },
        StackChangeFact::ProtectiveControlDiff {
            fact_id: "control-disabled".to_string(),
            diff_kind: AgentStackProtectionDiffKind::Disabled,
            roles: vec![AgentStackProtectionRole::Validation],
            confidence: AgentStackProtectionConfidence::High,
        },
    ]));

    assert_eq!(evaluation.decision, StackPolicyDecision::Block);
    assert_eq!(
        evaluation.winning_rule_ids,
        vec!["ASC-012-BLOCK-PROTECTIVE-CONTROL-WEAKENED"]
    );
    assert!(evaluation.conflicted);
    assert_eq!(
        evaluation
            .precedence
            .iter()
            .map(|entry| entry.decision)
            .collect::<Vec<_>>(),
        vec![
            StackPolicyDecision::Block,
            StackPolicyDecision::Review,
            StackPolicyDecision::Promote
        ]
    );
}

#[test]
fn stack_policy_blocks_missing_required_evidence_before_promoting() {
    let evidence = StackEvidenceCompleteness {
        available: vec![
            StackEvidenceKind::StructuralDiff,
            StackEvidenceKind::CapabilityEvidence,
            StackEvidenceKind::ProtectiveControlDiff,
            StackEvidenceKind::ValidationStatus,
        ],
        missing: vec![StackEvidenceKind::ObservedCapabilityUse],
    };
    let evaluation = reference_eval(
        StackPolicyFacts::new(
            evidence,
            [StackChangeFact::ValidationStatus {
                fact_id: "validation-pass".to_string(),
                check_id: "cargo-test".to_string(),
                passed: true,
            }],
        )
        .expect("facts should validate"),
    );

    assert_eq!(evaluation.decision, StackPolicyDecision::Block);
    assert_eq!(
        evaluation.winning_rule_ids,
        vec!["ASC-012-BLOCK-MISSING-REQUIRED-EVIDENCE"]
    );
    assert!(evaluation.matched_rules.iter().any(|rule_match| {
        rule_match.rule_id == "ASC-012-BLOCK-MISSING-REQUIRED-EVIDENCE"
            && rule_match
                .reason
                .contains(StackEvidenceKind::ObservedCapabilityUse.as_str())
    }));
}

#[test]
fn stack_policy_json_consumes_versioned_facts() {
    let input = serde_json::json!({
        "schema_version": STACK_POLICY_FACTS_SCHEMA_VERSION,
        "evidence": {
            "available": [
                "structural_diff",
                "capability_evidence",
                "observed_capability_use",
                "protective_control_diff",
                "validation_status"
            ],
            "missing": []
        },
        "facts": [{
            "kind": "capability_evidence",
            "fact_id": "cap-secret-observed",
            "component_id": "runtime:agent_runtime:runtime_profile/codex-default",
            "evidence_class": "observed",
            "capability": "secret_read",
            "trust_level": "runner_observed"
        }]
    })
    .to_string();

    let evaluation = StackPolicyEngine::conservative_reference()
        .evaluate_json(&input)
        .expect("versioned fact JSON should evaluate");

    assert_eq!(evaluation.decision, StackPolicyDecision::Block);
    assert_eq!(
        evaluation.winning_rule_ids,
        vec!["ASC-012-BLOCK-HIGH-IMPACT-AUTHORITY-GRANTED-OR-USED"]
    );
}

#[test]
fn stack_policy_parse_errors_are_engine_errors() {
    let error =
        StackPolicyEngine::from_json("{not json").expect_err("invalid policy JSON should fail");

    assert!(matches!(error, StackPolicyError::EngineParse { .. }));
}

#[test]
fn stack_policy_evaluation_errors_are_engine_errors() {
    let policy = StackPolicyDocument {
        schema_version: crate::stack_policy::STACK_POLICY_SCHEMA_VERSION.to_string(),
        required_evidence: Vec::new(),
        rules: vec![crate::stack_policy::StackPolicyRule {
            id: "CUSTOM-REVIEW-FAILED-VALIDATION".to_string(),
            decision: StackPolicyDecision::Review,
            reason: "failed validation needs review".to_string(),
            matches: crate::stack_policy::StackPolicyMatcher::ValidationStatus {
                passed: Some(false),
            },
        }],
    };
    let engine = StackPolicyEngine::from_policy(policy).expect("policy should validate");
    let error = engine
        .evaluate(&facts([StackChangeFact::ValidationStatus {
            fact_id: "validation-pass".to_string(),
            check_id: "cargo-test".to_string(),
            passed: true,
        }]))
        .expect_err("no matching rule should be an evaluation error");

    assert!(matches!(error, StackPolicyError::EngineEvaluation { .. }));
}
