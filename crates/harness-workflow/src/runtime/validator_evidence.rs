//! The evidence side of the transition contract.
//!
//! `allow` says which commands may drive a transition; `require_evidence` says
//! what must be proven for it to mint a fact. Splitting the two builders out
//! keeps `validator.rs` focused on the transition tables themselves.

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

    /// Drop every declared evidence requirement, leaving the allowed-command
    /// contract intact. Backs the completion-evidence kill switch.
    pub fn without_required_evidence(mut self) -> Self {
        for rule in &mut self.rules {
            rule.required_evidence.clear();
        }
        self
    }
}
