use harness_core::stack::capability_evidence::AgentStackCapabilityEvidenceClass;
use harness_core::stack::{
    AgentStackCapability, AgentStackComponentKind, AgentStackProtectionConfidence,
    AgentStackProtectionDiffKind, AgentStackProtectionRole, AgentStackSourceScope,
    AgentStackTrustLevel,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashSet};
use thiserror::Error;

mod reference;
mod validation;
pub use reference::conservative_reference_policy;
use validation::*;

pub const STACK_POLICY_SCHEMA_VERSION: &str = "harness-stack-policy/v0.1";
pub const STACK_POLICY_FACTS_SCHEMA_VERSION: &str = "harness-stack-policy-facts/v0.1";

const MISSING_EVIDENCE_RULE_ID: &str = "ASC-012-BLOCK-MISSING-REQUIRED-EVIDENCE";

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum StackPolicyError {
    #[error("stack policy engine parse error: {message}")]
    EngineParse { message: String },
    #[error("stack policy engine evaluation error: {message}")]
    EngineEvaluation { message: String },
}

impl StackPolicyError {
    fn parse(message: impl Into<String>) -> Self {
        Self::EngineParse {
            message: message.into(),
        }
    }

    fn evaluation(message: impl Into<String>) -> Self {
        Self::EngineEvaluation {
            message: message.into(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StackPolicyDecision {
    Promote,
    Review,
    Block,
}

impl StackPolicyDecision {
    pub const fn precedence(self) -> u16 {
        match self {
            Self::Promote => 10,
            Self::Review => 50,
            Self::Block => 100,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Promote => "promote",
            Self::Review => "review",
            Self::Block => "block",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StackEvidenceKind {
    StructuralDiff,
    CapabilityEvidence,
    ObservedCapabilityUse,
    ProtectiveControlDiff,
    ValidationStatus,
}

impl StackEvidenceKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::StructuralDiff => "structural_diff",
            Self::CapabilityEvidence => "capability_evidence",
            Self::ObservedCapabilityUse => "observed_capability_use",
            Self::ProtectiveControlDiff => "protective_control_diff",
            Self::ValidationStatus => "validation_status",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StackEvidenceCompleteness {
    #[serde(default)]
    pub available: Vec<StackEvidenceKind>,
    #[serde(default)]
    pub missing: Vec<StackEvidenceKind>,
}

impl StackEvidenceCompleteness {
    pub fn complete(kinds: impl IntoIterator<Item = StackEvidenceKind>) -> Self {
        Self {
            available: kinds.into_iter().collect(),
            missing: Vec::new(),
        }
    }

    pub fn is_available(&self, kind: StackEvidenceKind) -> bool {
        self.available.contains(&kind) && !self.missing.contains(&kind)
    }

    fn validate(&self) -> Result<(), StackPolicyError> {
        ensure_unique_evidence("available evidence", &self.available)?;
        ensure_unique_evidence("missing evidence", &self.missing)?;
        for kind in &self.available {
            if self.missing.contains(kind) {
                return Err(StackPolicyError::evaluation(format!(
                    "evidence kind `{}` cannot be both available and missing",
                    kind.as_str()
                )));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StackPolicyFacts {
    pub schema_version: String,
    pub evidence: StackEvidenceCompleteness,
    #[serde(default)]
    pub facts: Vec<StackChangeFact>,
}

impl StackPolicyFacts {
    pub fn new(
        evidence: StackEvidenceCompleteness,
        facts: impl IntoIterator<Item = StackChangeFact>,
    ) -> Result<Self, StackPolicyError> {
        let facts = Self {
            schema_version: STACK_POLICY_FACTS_SCHEMA_VERSION.to_string(),
            evidence,
            facts: facts.into_iter().collect(),
        };
        facts.validate()?;
        Ok(facts)
    }

    pub fn from_json(input: &str) -> Result<Self, StackPolicyError> {
        let facts: Self = serde_json::from_str(input).map_err(|error| {
            StackPolicyError::parse(format!("failed to parse stack policy facts JSON: {error}"))
        })?;
        facts.validate()?;
        Ok(facts)
    }

    pub fn validate(&self) -> Result<(), StackPolicyError> {
        if self.schema_version != STACK_POLICY_FACTS_SCHEMA_VERSION {
            return Err(StackPolicyError::evaluation(format!(
                "unsupported stack policy facts schema `{}`; expected `{STACK_POLICY_FACTS_SCHEMA_VERSION}`",
                self.schema_version
            )));
        }
        self.evidence.validate()?;

        let mut ids = HashSet::new();
        for fact in &self.facts {
            let fact_id = fact.fact_id();
            if fact_id.trim().is_empty() {
                return Err(StackPolicyError::evaluation(
                    "stack policy fact_id cannot be empty",
                ));
            }
            if !ids.insert(fact_id) {
                return Err(StackPolicyError::evaluation(format!(
                    "duplicate stack policy fact_id `{fact_id}`"
                )));
            }
            fact.validate()?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum StackChangeFact {
    ComponentAdded {
        fact_id: String,
        component_id: String,
        component_kind: AgentStackComponentKind,
        source_scope: AgentStackSourceScope,
    },
    ComponentRemoved {
        fact_id: String,
        component_id: String,
        component_kind: AgentStackComponentKind,
        source_scope: AgentStackSourceScope,
    },
    ComponentModified {
        fact_id: String,
        component_id: String,
        changed_fields: Vec<StackChangedField>,
    },
    CapabilityEvidence {
        fact_id: String,
        component_id: String,
        evidence_class: AgentStackCapabilityEvidenceClass,
        capability: AgentStackCapability,
        trust_level: AgentStackTrustLevel,
    },
    ProtectiveControlDiff {
        fact_id: String,
        diff_kind: AgentStackProtectionDiffKind,
        roles: Vec<AgentStackProtectionRole>,
        confidence: AgentStackProtectionConfidence,
    },
    ValidationStatus {
        fact_id: String,
        check_id: String,
        passed: bool,
    },
}

impl StackChangeFact {
    pub fn fact_id(&self) -> &str {
        match self {
            Self::ComponentAdded { fact_id, .. }
            | Self::ComponentRemoved { fact_id, .. }
            | Self::ComponentModified { fact_id, .. }
            | Self::CapabilityEvidence { fact_id, .. }
            | Self::ProtectiveControlDiff { fact_id, .. }
            | Self::ValidationStatus { fact_id, .. } => fact_id,
        }
    }

    pub const fn fact_kind(&self) -> StackFactKind {
        match self {
            Self::ComponentAdded { .. } => StackFactKind::ComponentAdded,
            Self::ComponentRemoved { .. } => StackFactKind::ComponentRemoved,
            Self::ComponentModified { .. } => StackFactKind::ComponentModified,
            Self::CapabilityEvidence { .. } => StackFactKind::CapabilityEvidence,
            Self::ProtectiveControlDiff { .. } => StackFactKind::ProtectiveControlDiff,
            Self::ValidationStatus { .. } => StackFactKind::ValidationStatus,
        }
    }

    fn validate(&self) -> Result<(), StackPolicyError> {
        match self {
            Self::ComponentAdded {
                component_id,
                component_kind,
                source_scope,
                ..
            }
            | Self::ComponentRemoved {
                component_id,
                component_kind,
                source_scope,
                ..
            } => {
                validate_component_identity(component_id, Some((*source_scope, *component_kind)))?;
            }
            Self::ComponentModified { component_id, .. } => {
                validate_component_identity(component_id, None)?;
            }
            Self::CapabilityEvidence {
                component_id,
                evidence_class,
                trust_level,
                ..
            } => {
                validate_component_identity(component_id, None)?;
                validate_capability_evidence_trust(*evidence_class, *trust_level)?;
            }
            Self::ProtectiveControlDiff { roles, .. } => {
                if roles.is_empty() {
                    return Err(StackPolicyError::evaluation(
                        "protective control facts must include at least one role",
                    ));
                }
            }
            Self::ValidationStatus { check_id, .. } => ensure_non_empty("check_id", check_id)?,
        }

        if let Self::ComponentModified { changed_fields, .. } = self {
            if changed_fields.is_empty() {
                return Err(StackPolicyError::evaluation(
                    "component modification facts must include changed_fields",
                ));
            }
            ensure_unique_fields(changed_fields)?;
        }

        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StackChangedField {
    Runtime,
    Context,
    Capability,
    Trust,
    Freshness,
    Validation,
    Integrity,
    Source,
    SelectionState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StackFactKind {
    ComponentAdded,
    ComponentRemoved,
    ComponentModified,
    CapabilityEvidence,
    ProtectiveControlDiff,
    ValidationStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StackPolicyDocument {
    pub schema_version: String,
    #[serde(default)]
    pub required_evidence: Vec<StackEvidenceKind>,
    pub rules: Vec<StackPolicyRule>,
}

impl StackPolicyDocument {
    pub fn from_json(input: &str) -> Result<Self, StackPolicyError> {
        let policy: Self = serde_json::from_str(input).map_err(|error| {
            StackPolicyError::parse(format!("failed to parse stack policy JSON: {error}"))
        })?;
        policy.validate()?;
        Ok(policy)
    }

    pub fn validate(&self) -> Result<(), StackPolicyError> {
        if self.schema_version != STACK_POLICY_SCHEMA_VERSION {
            return Err(StackPolicyError::parse(format!(
                "unsupported stack policy schema `{}`; expected `{STACK_POLICY_SCHEMA_VERSION}`",
                self.schema_version
            )));
        }
        ensure_unique_policy_evidence("required evidence", &self.required_evidence)?;
        if self.rules.is_empty() {
            return Err(StackPolicyError::parse(
                "stack policy must declare at least one rule",
            ));
        }

        let mut ids = HashSet::new();
        for rule in &self.rules {
            ensure_policy_non_empty("rule id", &rule.id)?;
            ensure_policy_non_empty("rule reason", &rule.reason)?;
            if rule.id == MISSING_EVIDENCE_RULE_ID {
                return Err(StackPolicyError::parse(format!(
                    "stack policy rule id `{MISSING_EVIDENCE_RULE_ID}` is reserved"
                )));
            }
            if !ids.insert(rule.id.as_str()) {
                return Err(StackPolicyError::parse(format!(
                    "duplicate stack policy rule id `{}`",
                    rule.id
                )));
            }
            rule.matches.validate()?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StackPolicyRule {
    pub id: String,
    pub decision: StackPolicyDecision,
    pub reason: String,
    #[serde(rename = "match")]
    pub matches: StackPolicyMatcher,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum StackPolicyMatcher {
    Always,
    FactKind {
        fact_kinds: Vec<StackFactKind>,
    },
    CapabilityEvidence {
        capabilities: Vec<AgentStackCapability>,
        evidence_classes: Vec<AgentStackCapabilityEvidenceClass>,
    },
    ProtectiveControlDiff {
        diff_kinds: Vec<AgentStackProtectionDiffKind>,
        #[serde(default)]
        roles: Vec<AgentStackProtectionRole>,
        #[serde(default)]
        confidences: Vec<AgentStackProtectionConfidence>,
    },
    ValidationStatus {
        passed: Option<bool>,
    },
    MissingEvidence {
        evidence: Vec<StackEvidenceKind>,
    },
    Any {
        conditions: Vec<StackPolicyMatcher>,
    },
    All {
        conditions: Vec<StackPolicyMatcher>,
    },
}

impl StackPolicyMatcher {
    fn validate(&self) -> Result<(), StackPolicyError> {
        match self {
            Self::Always | Self::ValidationStatus { .. } => Ok(()),
            Self::FactKind { fact_kinds } => ensure_non_empty_list("fact_kinds", fact_kinds),
            Self::CapabilityEvidence {
                capabilities,
                evidence_classes,
            } => {
                ensure_non_empty_list("capabilities", capabilities)?;
                ensure_non_empty_list("evidence_classes", evidence_classes)
            }
            Self::ProtectiveControlDiff {
                diff_kinds,
                roles: _,
                confidences: _,
            } => ensure_non_empty_list("diff_kinds", diff_kinds),
            Self::MissingEvidence { evidence } => ensure_non_empty_list("evidence", evidence),
            Self::Any { conditions } | Self::All { conditions } => {
                ensure_non_empty_list("conditions", conditions)?;
                for condition in conditions {
                    condition.validate()?;
                }
                Ok(())
            }
        }
    }

    fn matches(&self, facts: &StackPolicyFacts) -> Option<Vec<String>> {
        match self {
            Self::Always => Some(Vec::new()),
            Self::FactKind { fact_kinds } => {
                let matched = facts
                    .facts
                    .iter()
                    .filter(|fact| fact_kinds.contains(&fact.fact_kind()))
                    .map(|fact| fact.fact_id().to_owned())
                    .collect::<Vec<_>>();
                non_empty_match(matched)
            }
            Self::CapabilityEvidence {
                capabilities,
                evidence_classes,
            } => {
                let matched = facts
                    .facts
                    .iter()
                    .filter_map(|fact| match fact {
                        StackChangeFact::CapabilityEvidence {
                            fact_id,
                            evidence_class,
                            capability,
                            ..
                        } if capabilities.contains(capability)
                            && evidence_classes.contains(evidence_class) =>
                        {
                            Some(fact_id.clone())
                        }
                        _ => None,
                    })
                    .collect::<Vec<_>>();
                non_empty_match(matched)
            }
            Self::ProtectiveControlDiff {
                diff_kinds,
                roles,
                confidences,
            } => {
                let matched = facts
                    .facts
                    .iter()
                    .filter_map(|fact| match fact {
                        StackChangeFact::ProtectiveControlDiff {
                            fact_id,
                            diff_kind,
                            roles: fact_roles,
                            confidence,
                        } if diff_kinds.contains(diff_kind)
                            && (roles.is_empty()
                                || roles.iter().any(|role| fact_roles.contains(role)))
                            && (confidences.is_empty() || confidences.contains(confidence)) =>
                        {
                            Some(fact_id.clone())
                        }
                        _ => None,
                    })
                    .collect::<Vec<_>>();
                non_empty_match(matched)
            }
            Self::ValidationStatus { passed } => {
                let matched = facts
                    .facts
                    .iter()
                    .filter_map(|fact| match fact {
                        StackChangeFact::ValidationStatus {
                            fact_id,
                            passed: fact_passed,
                            ..
                        } if passed.is_none_or(|expected| expected == *fact_passed) => {
                            Some(fact_id.clone())
                        }
                        _ => None,
                    })
                    .collect::<Vec<_>>();
                non_empty_match(matched)
            }
            Self::MissingEvidence { evidence } => evidence
                .iter()
                .any(|kind| !facts.evidence.is_available(*kind))
                .then(Vec::new),
            Self::Any { conditions } => {
                let mut matched = BTreeSet::new();
                let mut any_matched = false;
                for condition in conditions {
                    if let Some(fact_ids) = condition.matches(facts) {
                        any_matched = true;
                        matched.extend(fact_ids);
                    }
                }
                any_matched.then(|| matched.into_iter().collect())
            }
            Self::All { conditions } => {
                let mut matched = BTreeSet::new();
                for condition in conditions {
                    let fact_ids = condition.matches(facts)?;
                    matched.extend(fact_ids);
                }
                Some(matched.into_iter().collect())
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct StackPolicyRuleMatch {
    pub rule_id: String,
    pub decision: StackPolicyDecision,
    pub matched_facts: Vec<String>,
    pub reason: String,
    pub precedence: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct StackPolicyPrecedenceEntry {
    pub decision: StackPolicyDecision,
    pub precedence: u16,
    pub reason: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct StackPolicyEvaluation {
    pub decision: StackPolicyDecision,
    pub winning_precedence: u16,
    pub winning_rule_ids: Vec<String>,
    pub conflicted: bool,
    pub precedence: Vec<StackPolicyPrecedenceEntry>,
    pub matched_rules: Vec<StackPolicyRuleMatch>,
}

#[derive(Debug, Clone)]
pub struct StackPolicyEngine {
    policy: StackPolicyDocument,
}

impl StackPolicyEngine {
    pub fn from_policy(policy: StackPolicyDocument) -> Result<Self, StackPolicyError> {
        policy.validate()?;
        Ok(Self { policy })
    }

    pub fn from_json(input: &str) -> Result<Self, StackPolicyError> {
        Self::from_policy(StackPolicyDocument::from_json(input)?)
    }

    pub fn conservative_reference() -> Self {
        Self::from_policy(conservative_reference_policy())
            .expect("conservative reference policy should validate")
    }

    pub fn policy(&self) -> &StackPolicyDocument {
        &self.policy
    }

    pub fn evaluate_json(&self, input: &str) -> Result<StackPolicyEvaluation, StackPolicyError> {
        self.evaluate(&StackPolicyFacts::from_json(input)?)
    }

    pub fn evaluate(
        &self,
        facts: &StackPolicyFacts,
    ) -> Result<StackPolicyEvaluation, StackPolicyError> {
        facts.validate()?;

        let mut matched_rules = self.missing_required_evidence_matches(facts);
        for rule in &self.policy.rules {
            if let Some(matched_facts) = rule.matches.matches(facts) {
                matched_rules.push(StackPolicyRuleMatch {
                    rule_id: rule.id.clone(),
                    decision: rule.decision,
                    matched_facts,
                    reason: rule.reason.clone(),
                    precedence: rule.decision.precedence(),
                });
            }
        }

        let winning_precedence = matched_rules
            .iter()
            .map(|rule_match| rule_match.precedence)
            .max()
            .ok_or_else(|| {
                StackPolicyError::evaluation(
                    "stack policy produced no decision; add an explicit promote/review/block rule",
                )
            })?;
        let decision = matched_rules
            .iter()
            .find(|rule_match| rule_match.precedence == winning_precedence)
            .map(|rule_match| rule_match.decision)
            .ok_or_else(|| StackPolicyError::evaluation("internal missing winning rule"))?;
        let winning_rule_ids = matched_rules
            .iter()
            .filter(|rule_match| rule_match.precedence == winning_precedence)
            .map(|rule_match| rule_match.rule_id.clone())
            .collect();
        let has_review = matched_rules
            .iter()
            .any(|rule_match| rule_match.decision == StackPolicyDecision::Review);
        let has_block = matched_rules
            .iter()
            .any(|rule_match| rule_match.decision == StackPolicyDecision::Block);

        Ok(StackPolicyEvaluation {
            decision,
            winning_precedence,
            winning_rule_ids,
            conflicted: has_review && has_block,
            precedence: precedence_entries(),
            matched_rules,
        })
    }

    fn missing_required_evidence_matches(
        &self,
        facts: &StackPolicyFacts,
    ) -> Vec<StackPolicyRuleMatch> {
        let missing = self
            .policy
            .required_evidence
            .iter()
            .filter(|kind| !facts.evidence.is_available(**kind))
            .map(|kind| kind.as_str())
            .collect::<Vec<_>>();

        if missing.is_empty() {
            return Vec::new();
        }

        vec![StackPolicyRuleMatch {
            rule_id: MISSING_EVIDENCE_RULE_ID.to_string(),
            decision: StackPolicyDecision::Block,
            matched_facts: Vec::new(),
            reason: format!("required evidence is unavailable: {}", missing.join(", ")),
            precedence: StackPolicyDecision::Block.precedence(),
        }]
    }
}

fn precedence_entries() -> Vec<StackPolicyPrecedenceEntry> {
    vec![
        StackPolicyPrecedenceEntry {
            decision: StackPolicyDecision::Block,
            precedence: StackPolicyDecision::Block.precedence(),
            reason: "block has the highest precedence",
        },
        StackPolicyPrecedenceEntry {
            decision: StackPolicyDecision::Review,
            precedence: StackPolicyDecision::Review.precedence(),
            reason: "review wins over promote when no block rule matched",
        },
        StackPolicyPrecedenceEntry {
            decision: StackPolicyDecision::Promote,
            precedence: StackPolicyDecision::Promote.precedence(),
            reason: "promote applies only when no block or review rule matched",
        },
    ]
}

fn non_empty_match(matched: Vec<String>) -> Option<Vec<String>> {
    (!matched.is_empty()).then_some(matched)
}
