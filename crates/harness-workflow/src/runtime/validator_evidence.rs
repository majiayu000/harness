//! The evidence side of the transition contract.
//!
//! `allow` says which commands may drive a transition; `require_evidence` says
//! what must be proven for it to mint a fact. Splitting the two builders out
//! keeps `validator.rs` focused on the transition tables themselves.

use super::super::completion_evidence;
use super::TransitionAllowlist;
use harness_core::claim_trust::ClaimTrustLevel;

impl TransitionAllowlist {
    pub fn github_issue_pr_v1_defaults() -> Self {
        use super::super::WorkflowCommandType::{
            BindPr, EnqueueActivity, MarkBlocked, MarkCancelled, MarkDone, MarkFailed,
            RecordPlanConcern, RequestOperatorAttention, StartChildWorkflow, Wait,
        };

        Self::default()
            .allow("discovered", "awaiting_dependencies", [Wait])
            .allow("failed", "awaiting_dependencies", [Wait])
            .allow("cancelled", "awaiting_dependencies", [Wait])
            .allow("awaiting_dependencies", "awaiting_dependencies", [Wait])
            .allow(
                "awaiting_dependencies",
                "scheduled",
                [EnqueueActivity, Wait],
            )
            .allow("awaiting_dependencies", "planning", [EnqueueActivity, Wait])
            .allow(
                "awaiting_dependencies",
                "implementing",
                [EnqueueActivity, Wait],
            )
            .allow("discovered", "scheduled", [EnqueueActivity, Wait])
            .allow("discovered", "planning", [EnqueueActivity, Wait])
            .allow("discovered", "implementing", [EnqueueActivity, Wait])
            .allow("scheduled", "scheduled", [EnqueueActivity, Wait])
            .allow("failed", "scheduled", [EnqueueActivity, Wait])
            .allow("failed", "planning", [EnqueueActivity, Wait])
            .allow("failed", "implementing", [EnqueueActivity, Wait])
            .allow("failed", "replanning", [EnqueueActivity, Wait])
            .allow("failed", "local_review_gate", [EnqueueActivity, Wait])
            .allow(
                "failed",
                "awaiting_feedback",
                [EnqueueActivity, StartChildWorkflow, Wait],
            )
            .allow(
                "failed",
                "addressing_feedback",
                [EnqueueActivity, StartChildWorkflow, Wait],
            )
            .allow("failed", "merging", [EnqueueActivity])
            .allow("blocked", "implementing", [EnqueueActivity, Wait])
            .allow("blocked", "replanning", [EnqueueActivity, Wait])
            .allow("blocked", "local_review_gate", [EnqueueActivity, Wait])
            .allow(
                "blocked",
                "awaiting_feedback",
                [EnqueueActivity, StartChildWorkflow, Wait],
            )
            .allow(
                "blocked",
                "addressing_feedback",
                [EnqueueActivity, StartChildWorkflow, Wait],
            )
            .allow("blocked", "merging", [EnqueueActivity])
            .allow("cancelled", "scheduled", [EnqueueActivity, Wait])
            .allow("cancelled", "planning", [EnqueueActivity, Wait])
            .allow("cancelled", "implementing", [EnqueueActivity, Wait])
            .allow("scheduled", "planning", [EnqueueActivity, Wait])
            .allow(
                "scheduled",
                "implementing",
                [EnqueueActivity, RecordPlanConcern, Wait],
            )
            .allow(
                "scheduled",
                "replanning",
                [EnqueueActivity, RecordPlanConcern, MarkBlocked, Wait],
            )
            .allow("planning", "implementing", [EnqueueActivity, MarkBlocked])
            .allow("planning", "planning", [EnqueueActivity, Wait])
            .allow(
                "implementing",
                "implementing",
                [EnqueueActivity, RecordPlanConcern, Wait],
            )
            .allow(
                "implementing",
                "replanning",
                [EnqueueActivity, RecordPlanConcern, MarkBlocked, Wait],
            )
            .allow(
                "replanning",
                "implementing",
                [EnqueueActivity, RecordPlanConcern, MarkBlocked, Wait],
            )
            .allow(
                "implementing",
                "pr_open",
                [BindPr, EnqueueActivity, StartChildWorkflow, Wait],
            )
            .allow("implementing", "done", [MarkDone])
            .allow(
                "scheduled",
                "pr_open",
                [BindPr, EnqueueActivity, StartChildWorkflow, Wait],
            )
            .allow("pr_open", "pr_open", [BindPr, Wait])
            .allow("pr_open", "local_review_gate", [EnqueueActivity, Wait])
            .allow("pr_open", "awaiting_feedback", [Wait])
            .allow(
                "awaiting_feedback",
                "local_review_gate",
                [EnqueueActivity, Wait],
            )
            .allow(
                "local_review_gate",
                "local_review_gate",
                [EnqueueActivity, Wait],
            )
            .allow("local_review_gate", "awaiting_feedback", [Wait])
            .allow(
                "local_review_gate",
                "addressing_feedback",
                [EnqueueActivity, MarkBlocked, Wait],
            )
            .allow("pr_open", "done", [MarkDone])
            .allow(
                "awaiting_feedback",
                "awaiting_feedback",
                [EnqueueActivity, StartChildWorkflow, Wait],
            )
            .allow(
                "awaiting_feedback",
                "addressing_feedback",
                [EnqueueActivity, StartChildWorkflow, MarkBlocked, Wait],
            )
            .allow(
                "addressing_feedback",
                "addressing_feedback",
                [EnqueueActivity, StartChildWorkflow, MarkBlocked, Wait],
            )
            .allow(
                "addressing_feedback",
                "local_review_gate",
                [EnqueueActivity, StartChildWorkflow, Wait],
            )
            .allow(
                "awaiting_feedback",
                "quality_gate_pending",
                [StartChildWorkflow, Wait],
            )
            .allow(
                "quality_gate_pending",
                "ready_to_merge",
                std::iter::empty::<super::super::WorkflowCommandType>(),
            )
            .allow("awaiting_feedback", "done", [MarkDone])
            .allow("addressing_feedback", "done", [MarkDone])
            .allow("quality_gate_pending", "done", [MarkDone])
            .allow("quality_gate_pending", "quality_gate_pending", [Wait])
            .allow("ready_to_merge", "ready_to_merge", [Wait])
            .allow("ready_to_merge", "merging", [EnqueueActivity])
            .allow("merging", "done", [MarkDone])
            .allow("ready_to_merge", "done", [MarkDone])
            .allow_from_any("blocked", [MarkBlocked, RequestOperatorAttention, Wait])
            .allow_from_any("failed", [MarkFailed])
            .allow_from_any("cancelled", [MarkCancelled])
            .require_evidence_with_trust(
                "implementing",
                "pr_open",
                [(
                    completion_evidence::EVIDENCE_VERIFIED_PR_BINDING,
                    ClaimTrustLevel::RuntimeObserved,
                )],
            )
            .require_evidence_into_with_trust(
                "done",
                [(
                    completion_evidence::EVIDENCE_GITHUB_TERMINAL,
                    ClaimTrustLevel::RuntimeObserved,
                )],
            )
    }

    /// Attach required evidence classes to an already-allowed transition.
    ///
    /// Transition tables are compile-time constants, so a missing target rule
    /// is a definition bug that must surface at registry construction rather
    /// than silently leave the transition unguarded.
    pub fn require_evidence(
        mut self,
        from_state: impl Into<String>,
        to_state: impl Into<String>,
        evidence: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        let from_state = from_state.into();
        let to_state = to_state.into();
        let rule = self
            .rules
            .iter_mut()
            .find(|rule| {
                rule.from_state.as_deref() == Some(from_state.as_str()) && rule.to_state == to_state
            })
            .unwrap_or_else(|| {
                panic!(
                    "cannot require evidence for unallowed transition '{from_state}' -> '{to_state}'"
                )
            });
        require_evidence_on_rule(
            rule,
            evidence
                .into_iter()
                .map(|kind| (kind.into(), ClaimTrustLevel::SelfDeclared)),
        );
        self
    }

    /// Attach required evidence classes and their minimum trust to an already
    /// allowed transition.
    pub fn require_evidence_with_trust(
        mut self,
        from_state: impl Into<String>,
        to_state: impl Into<String>,
        evidence: impl IntoIterator<Item = (impl Into<String>, ClaimTrustLevel)>,
    ) -> Self {
        let from_state = from_state.into();
        let to_state = to_state.into();
        let rule = self
            .rules
            .iter_mut()
            .find(|rule| {
                rule.from_state.as_deref() == Some(from_state.as_str()) && rule.to_state == to_state
            })
            .unwrap_or_else(|| {
                panic!(
                    "cannot require evidence for unallowed transition '{from_state}' -> '{to_state}'"
                )
            });
        require_evidence_on_rule(
            rule,
            evidence
                .into_iter()
                .map(|(kind, trust)| (kind.into(), trust)),
        );
        self
    }

    /// Attach required evidence to every explicitly-declared transition that
    /// lands on `to_state`.
    ///
    /// `allow_from_any` rules are deliberately excluded: they cover operator
    /// and terminal escapes that carry their own contracts, so requiring
    /// fact-minting evidence on them would block the very paths used to
    /// recover from a missing-evidence block.
    ///
    /// Like [`Self::require_evidence`], an unmatched target is a definition
    /// bug and panics at registry construction rather than silently leaving
    /// the transition unguarded.
    pub fn require_evidence_into<'a>(
        mut self,
        to_state: &str,
        evidence: impl IntoIterator<Item = &'a str>,
    ) -> Self {
        let evidence: Vec<String> = evidence.into_iter().map(str::to_string).collect();
        let mut matched = false;
        for rule in &mut self.rules {
            if rule.from_state.is_some() && rule.to_state == to_state {
                require_evidence_on_rule(
                    rule,
                    evidence
                        .iter()
                        .cloned()
                        .map(|kind| (kind, ClaimTrustLevel::SelfDeclared)),
                );
                matched = true;
            }
        }
        assert!(
            matched,
            "cannot require evidence for undeclared transition into '{to_state}'"
        );
        self
    }

    /// Attach required evidence and minimum trust to every explicitly-declared
    /// transition that lands on `to_state`.
    pub fn require_evidence_into_with_trust<'a>(
        mut self,
        to_state: &str,
        evidence: impl IntoIterator<Item = (&'a str, ClaimTrustLevel)>,
    ) -> Self {
        let evidence: Vec<(String, ClaimTrustLevel)> = evidence
            .into_iter()
            .map(|(kind, trust)| (kind.to_string(), trust))
            .collect();
        let mut matched = false;
        for rule in &mut self.rules {
            if rule.from_state.is_some() && rule.to_state == to_state {
                require_evidence_on_rule(rule, evidence.iter().cloned());
                matched = true;
            }
        }
        assert!(
            matched,
            "cannot require evidence for undeclared transition into '{to_state}'"
        );
        self
    }

    /// The GH-1766 evidence contract for `github_issue_pr`.
    ///
    /// Initial transitions from implementation or a detected candidate into
    /// `pr_scope_review` mint the fact "a PR exists for this work", so they
    /// require the server's own verification of the claimed PR. Later
    /// transitions from feedback states reuse that bound identity and only
    /// request a fresh classifier pass. Every declared path into `done`
    /// requires server-recognized terminal proof.
    pub fn with_github_issue_pr_evidence_contract(self) -> Self {
        self.require_evidence_with_trust(
            "implementing",
            "pr_scope_review",
            [(
                completion_evidence::EVIDENCE_VERIFIED_PR_BINDING,
                ClaimTrustLevel::RuntimeObserved,
            )],
        )
        .require_evidence_with_trust(
            "scheduled",
            "pr_scope_review",
            [(
                completion_evidence::EVIDENCE_VERIFIED_PR_BINDING,
                ClaimTrustLevel::RuntimeObserved,
            )],
        )
        .require_evidence_with_trust(
            "implementing",
            "pr_open",
            [(
                completion_evidence::EVIDENCE_VERIFIED_PR_BINDING,
                ClaimTrustLevel::RuntimeObserved,
            )],
        )
        .require_evidence_into_with_trust(
            "done",
            [(
                completion_evidence::EVIDENCE_GITHUB_TERMINAL,
                ClaimTrustLevel::RuntimeObserved,
            )],
        )
    }

    /// The GH-1766 evidence contract for `quality_gate`: Passed may be minted
    /// only from a server-executed validation digest, never from the agent's
    /// own claim that the commands succeeded.
    pub fn with_quality_gate_evidence_contract(self) -> Self {
        self.require_evidence_with_trust(
            "checking",
            "passed",
            [(
                completion_evidence::EVIDENCE_SERVER_VALIDATION_DIGEST,
                ClaimTrustLevel::Reexecuted,
            )],
        )
    }

    /// The GH-1766 evidence contract for `pr_feedback`: declaring a PR ready
    /// to merge requires a server-fetched PR snapshot.
    pub fn with_pr_feedback_evidence_contract(self) -> Self {
        self.require_evidence_with_trust(
            "inspecting",
            "ready_to_merge",
            [(
                completion_evidence::EVIDENCE_SERVER_PR_SNAPSHOT,
                ClaimTrustLevel::RuntimeObserved,
            )],
        )
    }

    /// Drop every declared evidence requirement, leaving the allowed-command
    /// contract intact. Backs the completion-evidence kill switch.
    pub fn without_required_evidence(mut self) -> Self {
        for rule in &mut self.rules {
            rule.required_evidence.clear();
            rule.required_evidence_trust.clear();
        }
        self
    }
}

fn require_evidence_on_rule(
    rule: &mut super::TransitionRule,
    evidence: impl IntoIterator<Item = (String, ClaimTrustLevel)>,
) {
    for (kind, trust) in evidence {
        rule.required_evidence.insert(kind.clone());
        rule.required_evidence_trust
            .entry(kind)
            .and_modify(|existing| {
                if trust > *existing {
                    *existing = trust;
                }
            })
            .or_insert(trust);
    }
}
