use super::{
    StackEvidenceKind, StackPolicyDecision, StackPolicyDocument, StackPolicyMatcher,
    StackPolicyRule, STACK_POLICY_SCHEMA_VERSION,
};
use harness_core::stack::capability_evidence::AgentStackCapabilityEvidenceClass;
use harness_core::stack::{AgentStackCapability, AgentStackProtectionDiffKind};

pub fn conservative_reference_policy() -> StackPolicyDocument {
    StackPolicyDocument {
        schema_version: STACK_POLICY_SCHEMA_VERSION.to_string(),
        required_evidence: vec![
            StackEvidenceKind::StructuralDiff,
            StackEvidenceKind::CapabilityEvidence,
            StackEvidenceKind::ObservedCapabilityUse,
            StackEvidenceKind::ProtectiveControlDiff,
            StackEvidenceKind::ValidationStatus,
        ],
        rules: vec![
            StackPolicyRule {
                id: "ASC-012-BLOCK-PROTECTIVE-CONTROL-WEAKENED".to_string(),
                decision: StackPolicyDecision::Block,
                reason: "protective control was removed, disabled, scope-reduced, or made fail-open"
                    .to_string(),
                matches: StackPolicyMatcher::ProtectiveControlDiff {
                    diff_kinds: vec![
                        AgentStackProtectionDiffKind::Removed,
                        AgentStackProtectionDiffKind::Disabled,
                        AgentStackProtectionDiffKind::ScopeReduced,
                        AgentStackProtectionDiffKind::FailOpen,
                    ],
                    roles: Vec::new(),
                    confidences: Vec::new(),
                },
            },
            StackPolicyRule {
                id: "ASC-012-BLOCK-HIGH-IMPACT-AUTHORITY-GRANTED-OR-USED".to_string(),
                decision: StackPolicyDecision::Block,
                reason: "destructive, secret, privileged, or production-write authority was granted or observed"
                    .to_string(),
                matches: StackPolicyMatcher::CapabilityEvidence {
                    capabilities: vec![
                        AgentStackCapability::Destructive,
                        AgentStackCapability::SecretRead,
                        AgentStackCapability::Privileged,
                        AgentStackCapability::ProductionWrite,
                    ],
                    evidence_classes: vec![
                        AgentStackCapabilityEvidenceClass::Granted,
                        AgentStackCapabilityEvidenceClass::Observed,
                    ],
                },
            },
            StackPolicyRule {
                id: "ASC-012-REVIEW-SENSITIVE-AUTHORITY-DECLARED".to_string(),
                decision: StackPolicyDecision::Review,
                reason: "sensitive authority changed and requires repository review".to_string(),
                matches: StackPolicyMatcher::Any {
                    conditions: vec![
                        StackPolicyMatcher::CapabilityEvidence {
                            capabilities: all_sensitive_capabilities(),
                            evidence_classes: vec![AgentStackCapabilityEvidenceClass::Declared],
                        },
                        StackPolicyMatcher::CapabilityEvidence {
                            capabilities: vec![
                                AgentStackCapability::Network,
                                AgentStackCapability::Shell,
                                AgentStackCapability::FileWrite,
                            ],
                            evidence_classes: vec![
                                AgentStackCapabilityEvidenceClass::Granted,
                                AgentStackCapabilityEvidenceClass::Observed,
                            ],
                        },
                    ],
                },
            },
            StackPolicyRule {
                id: "ASC-012-REVIEW-AMBIGUOUS-PROTECTIVE-CONTROL-EVIDENCE".to_string(),
                decision: StackPolicyDecision::Review,
                reason: "protective control evidence is ambiguous".to_string(),
                matches: StackPolicyMatcher::ProtectiveControlDiff {
                    diff_kinds: vec![AgentStackProtectionDiffKind::AmbiguousReviewEvidence],
                    roles: Vec::new(),
                    confidences: Vec::new(),
                },
            },
            StackPolicyRule {
                id: "ASC-012-REVIEW-VALIDATION-FAILED".to_string(),
                decision: StackPolicyDecision::Review,
                reason: "validation evidence contains a failed check".to_string(),
                matches: StackPolicyMatcher::ValidationStatus { passed: Some(false) },
            },
            StackPolicyRule {
                id: "ASC-012-PROMOTE-COMPLETE-EVIDENCE-NO-RISK".to_string(),
                decision: StackPolicyDecision::Promote,
                reason: "required evidence is complete and no higher-precedence risk rule matched"
                    .to_string(),
                matches: StackPolicyMatcher::Always,
            },
        ],
    }
}

fn all_sensitive_capabilities() -> Vec<AgentStackCapability> {
    vec![
        AgentStackCapability::Destructive,
        AgentStackCapability::SecretRead,
        AgentStackCapability::Network,
        AgentStackCapability::Privileged,
        AgentStackCapability::ProductionWrite,
        AgentStackCapability::Shell,
        AgentStackCapability::FileWrite,
    ]
}
