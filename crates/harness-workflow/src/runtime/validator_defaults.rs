use super::TransitionAllowlist;
use crate::runtime::completion_evidence::{
    EVIDENCE_GITHUB_TERMINAL, EVIDENCE_PROMPT_COMPLETION, EVIDENCE_SERVER_PR_SNAPSHOT,
    EVIDENCE_SERVER_VALIDATION_DIGEST, EVIDENCE_VERIFIED_PR_BINDING,
};
use crate::runtime::model::WorkflowCommandType;

impl TransitionAllowlist {
    /// Attach required evidence classes to every already-declared rule that
    /// matches `from_state -> to_state`. Panics when no such rule exists so a
    /// typo in an evidence contract fails at construction, not silently at
    /// runtime.
    pub fn require_evidence<'a>(
        mut self,
        from_state: &str,
        to_state: &str,
        evidence: impl IntoIterator<Item = &'a str>,
    ) -> Self {
        let evidence: Vec<String> = evidence.into_iter().map(str::to_string).collect();
        let mut matched = false;
        for rule in &mut self.rules {
            if rule.from_state.as_deref() == Some(from_state) && rule.to_state == to_state {
                rule.required_evidence.extend(evidence.iter().cloned());
                matched = true;
            }
        }
        assert!(
            matched,
            "require_evidence targets undeclared transition '{from_state}' -> '{to_state}'"
        );
        self
    }

    /// Attach required evidence classes to every already-declared explicit
    /// rule targeting `to_state`, regardless of source state. `from_any`
    /// rules are excluded: they cover operator/terminal escapes with their
    /// own contracts, and the reconciliation-only done path is validated
    /// separately with server-checked terminal evidence.
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
            "require_evidence_into targets undeclared transition into '{to_state}'"
        );
        self
    }

    pub fn github_issue_pr_defaults() -> Self {
        use WorkflowCommandType::{
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
                std::iter::empty::<WorkflowCommandType>(),
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
            .require_evidence("implementing", "pr_open", [EVIDENCE_VERIFIED_PR_BINDING])
            .require_evidence_into("done", [EVIDENCE_GITHUB_TERMINAL])
    }

    pub fn quality_gate_defaults() -> Self {
        use WorkflowCommandType::{
            EnqueueActivity, MarkBlocked, MarkCancelled, MarkFailed, RequestOperatorAttention, Wait,
        };

        Self::default()
            .allow("pending", "checking", [EnqueueActivity, Wait])
            .allow("checking", "checking", [EnqueueActivity, Wait])
            .allow(
                "checking",
                "passed",
                std::iter::empty::<WorkflowCommandType>(),
            )
            .allow_from_any("blocked", [MarkBlocked, RequestOperatorAttention, Wait])
            .allow_from_any("failed", [MarkFailed])
            .allow_from_any("cancelled", [MarkCancelled])
            .require_evidence("checking", "passed", [EVIDENCE_SERVER_VALIDATION_DIGEST])
    }

    pub fn pr_feedback_defaults() -> Self {
        use WorkflowCommandType::{
            EnqueueActivity, MarkBlocked, MarkCancelled, MarkFailed, RequestOperatorAttention, Wait,
        };

        Self::default()
            .allow("pending", "inspecting", [EnqueueActivity, Wait])
            .allow("inspecting", "inspecting", [EnqueueActivity, Wait])
            .allow("inspecting", "feedback_found", std::iter::empty())
            .allow("inspecting", "no_actionable_feedback", std::iter::empty())
            .allow("inspecting", "ready_to_merge", std::iter::empty())
            .allow("feedback_found", "done", [Wait])
            .allow("no_actionable_feedback", "done", [Wait])
            .allow("ready_to_merge", "done", [Wait])
            .allow_from_any("blocked", [MarkBlocked, RequestOperatorAttention, Wait])
            .allow_from_any("failed", [MarkFailed])
            .allow_from_any("cancelled", [MarkCancelled])
            .require_evidence(
                "inspecting",
                "ready_to_merge",
                [EVIDENCE_SERVER_PR_SNAPSHOT],
            )
    }

    pub fn prompt_task_defaults() -> Self {
        use WorkflowCommandType::{
            EnqueueActivity, MarkBlocked, MarkCancelled, MarkDone, MarkFailed,
            RequestOperatorAttention, Wait,
        };

        Self::default()
            .allow("submitted", "awaiting_dependencies", [Wait])
            .allow("failed", "awaiting_dependencies", [Wait])
            .allow("cancelled", "awaiting_dependencies", [Wait])
            .allow("awaiting_dependencies", "awaiting_dependencies", [Wait])
            .allow(
                "awaiting_dependencies",
                "implementing",
                [EnqueueActivity, Wait],
            )
            .allow("submitted", "implementing", [EnqueueActivity, Wait])
            .allow("failed", "implementing", [EnqueueActivity, Wait])
            .allow("cancelled", "implementing", [EnqueueActivity, Wait])
            .allow("implementing", "implementing", [EnqueueActivity])
            .allow("blocked", "awaiting_dependencies", [Wait])
            .allow("blocked", "implementing", [EnqueueActivity, Wait])
            .allow("implementing", "done", [MarkDone])
            .allow_from_any("blocked", [MarkBlocked, RequestOperatorAttention, Wait])
            .allow_from_any("failed", [MarkFailed])
            .allow_from_any("cancelled", [MarkCancelled])
            .require_evidence("implementing", "done", [EVIDENCE_PROMPT_COMPLETION])
    }
}
