//! The evidence side of the transition contract.
//!
//! `allow` says which commands may drive a transition; `require_evidence` says
//! what must be proven for it to mint a fact. Splitting the two builders out
//! keeps `validator.rs` focused on the transition tables themselves.

use super::super::completion_evidence;
use super::TransitionAllowlist;

impl TransitionAllowlist {
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
        rule.required_evidence
            .extend(evidence.into_iter().map(Into::into));
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
                rule.required_evidence.extend(evidence.iter().cloned());
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
    /// `implementing -> pr_open` mints the fact "a PR exists for this work",
    /// so it requires the server's own verification of the claimed PR rather
    /// than the agent's word. Every declared path into `done` requires
    /// server-recognized terminal proof.
    pub fn with_github_issue_pr_evidence_contract(self) -> Self {
        self.require_evidence(
            "implementing",
            "pr_open",
            [completion_evidence::EVIDENCE_VERIFIED_PR_BINDING],
        )
        .require_evidence_into("done", [completion_evidence::EVIDENCE_GITHUB_TERMINAL])
    }

    /// The GH-1766 evidence contract for `quality_gate`: Passed may be minted
    /// only from a server-executed validation digest, never from the agent's
    /// own claim that the commands succeeded.
    pub fn with_quality_gate_evidence_contract(self) -> Self {
        self.require_evidence(
            "checking",
            "passed",
            [completion_evidence::EVIDENCE_SERVER_VALIDATION_DIGEST],
        )
    }

    /// The GH-1766 evidence contract for `pr_feedback`: declaring a PR ready
    /// to merge requires a server-fetched PR snapshot.
    pub fn with_pr_feedback_evidence_contract(self) -> Self {
        self.require_evidence(
            "inspecting",
            "ready_to_merge",
            [completion_evidence::EVIDENCE_SERVER_PR_SNAPSHOT],
        )
    }

    /// Drop every declared evidence requirement, leaving the allowed-command
    /// contract intact. Backs the completion-evidence kill switch.
    pub fn without_required_evidence(mut self) -> Self {
        for rule in &mut self.rules {
            rule.required_evidence.clear();
        }
        self
    }
}
