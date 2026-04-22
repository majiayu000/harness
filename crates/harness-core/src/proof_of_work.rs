use serde::{Deserialize, Serialize};

/// Outcome of the external code review for a completed task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewOutcome {
    /// Reviewer approved the PR (LGTM).
    Approved,
    /// Reviewer requested changes but the task completed before full approval.
    ChangesRequested,
    /// No review rounds were executed.
    Skipped,
    /// Review was informational only (e.g. periodic_review, sprint_planner) — no PR was reviewed.
    NotApplicable,
}

/// Outcome of the CI/test gate for a completed task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CiStatus {
    /// Test gate passed (LGTM was accepted and task is Done).
    Passed,
    /// Task failed; indicates a test/CI or review failure.
    Failed,
    /// Status cannot be determined from available data.
    Unknown,
}

/// A key-value quality signal attached to a proof-of-work record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QualitySignal {
    pub label: String,
    pub value: String,
}

/// Durable proof-of-work summary for a completed task.
///
/// Aggregates completion evidence so operators can retrieve it without
/// reconstructing from raw logs or event history.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProofOfWork {
    pub task_id: String,
    pub pr_url: Option<String>,
    pub repo: Option<String>,
    pub issue: Option<u64>,
    pub ci_status: CiStatus,
    pub review_outcome: ReviewOutcome,
    /// Total number of review rounds executed.
    pub review_rounds: u32,
    /// Raw output from the final review round, if available.
    pub final_review_detail: Option<String>,
    /// Quality signals derived from the task.
    pub quality_signals: Vec<QualitySignal>,
}

impl ProofOfWork {
    pub fn with_quality_signals(mut self, signals: Vec<QualitySignal>) -> Self {
        self.quality_signals = signals;
        self
    }
}

/// Result label written by the review loop when the reviewer approves.
pub const RESULT_LGTM: &str = "lgtm";

/// Result label written by the review loop when the reviewer requests fixes.
pub const RESULT_FIXED: &str = "fixed";

/// Result label written by the pipeline when a non-PR review task completes.
pub const RESULT_COMPLETED: &str = "completed";

/// Result label written by the agent reviewer when it approves the PR.
pub const RESULT_APPROVED: &str = "approved";

/// Result label written when the external reviewer quota is exhausted.
pub const RESULT_QUOTA_EXHAUSTED: &str = "quota_exhausted";

/// Action label for review rounds (external bot path).
pub const ACTION_REVIEW: &str = "review";

/// Action label for agent-review rounds (used when review_bot_auto_trigger is disabled).
pub const ACTION_AGENT_REVIEW: &str = "agent_review";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn review_outcome_serde_roundtrip() {
        let approved = ReviewOutcome::Approved;
        let json = serde_json::to_string(&approved).unwrap();
        assert_eq!(json, "\"approved\"");
        let back: ReviewOutcome = serde_json::from_str(&json).unwrap();
        assert_eq!(back, ReviewOutcome::Approved);
    }

    #[test]
    fn ci_status_serde_roundtrip() {
        let passed = CiStatus::Passed;
        let json = serde_json::to_string(&passed).unwrap();
        assert_eq!(json, "\"passed\"");
        let back: CiStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(back, CiStatus::Passed);
    }

    #[test]
    fn proof_of_work_with_quality_signals() {
        let pow = ProofOfWork {
            task_id: "t1".to_string(),
            pr_url: None,
            repo: None,
            issue: None,
            ci_status: CiStatus::Unknown,
            review_outcome: ReviewOutcome::Skipped,
            review_rounds: 0,
            final_review_detail: None,
            quality_signals: Vec::new(),
        };
        let signals = vec![QualitySignal {
            label: "review_rounds".to_string(),
            value: "0".to_string(),
        }];
        let pow = pow.with_quality_signals(signals);
        assert_eq!(pow.quality_signals.len(), 1);
        assert_eq!(pow.quality_signals[0].label, "review_rounds");
    }
}
