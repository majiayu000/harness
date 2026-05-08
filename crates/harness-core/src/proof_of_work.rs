//! Machine-readable "proof of work" summary for a completed task.
//!
//! Issue #876: operators currently reconstruct completion evidence from raw
//! logs, PR state, and event history. The structures here package the same
//! evidence into a single response so callers can consume it without crossing
//! storage boundaries.
//!
//! These types are pure data — derivation lives in `harness-server` because
//! it requires knowledge of `TaskState`. Keeping the wire format here lets
//! API consumers depend only on `harness-core`.

use serde::{Deserialize, Serialize};

/// Outcome of the project's CI/test gate for the completed task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CiStatus {
    /// Local validation passed and the reviewer approved.
    Passed,
    /// The task ended in a failure, or a review round reported a
    /// quota / billing / upstream / timeout failure.
    Failed,
    /// No conclusive signal — for example, a cancelled task or one where
    /// the review loop never produced a terminal verdict.
    Unknown,
}

/// Reviewer-side verdict aggregated across all review rounds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewOutcome {
    /// The last review round was `lgtm`, or an agent-review round was
    /// explicitly `approved`.
    Approved,
    /// At least one review round requested changes and no later round
    /// approved the PR.
    ChangesRequested,
    /// No review round ran for this task.
    Skipped,
}

/// Free-form quality signal surfaced alongside the structured proof.
///
/// Kept open-ended so additional dimensions (complexity score, lint summary,
/// coverage delta) can be attached without a schema change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QualitySignal {
    pub name: String,
    pub value: String,
}

/// Single-payload proof-of-work summary for a completed task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProofOfWork {
    pub task_id: String,
    /// Task status at the time the proof was generated. Snake-case to match
    /// the on-the-wire representation of `TaskStatus` (e.g. `done`, `failed`).
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pr_url: Option<String>,
    pub ci_status: CiStatus,
    pub review_outcome: ReviewOutcome,
    /// Total number of review rounds the task went through. `0` when the
    /// task completed without entering the review phase.
    pub review_rounds: u32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub quality_signals: Vec<QualitySignal>,
}

impl ProofOfWork {
    pub fn with_quality_signals(mut self, signals: Vec<QualitySignal>) -> Self {
        self.quality_signals = signals;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proof_serde_roundtrip() {
        let proof = ProofOfWork {
            task_id: "task-1".into(),
            status: "done".into(),
            pr_url: Some("https://github.com/owner/repo/pull/42".into()),
            ci_status: CiStatus::Passed,
            review_outcome: ReviewOutcome::Approved,
            review_rounds: 3,
            quality_signals: vec![QualitySignal {
                name: "turns".into(),
                value: "3".into(),
            }],
        };
        let json = serde_json::to_string(&proof).unwrap();
        let decoded: ProofOfWork = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, proof);
    }

    #[test]
    fn empty_quality_signals_skipped_in_json() {
        let proof = ProofOfWork {
            task_id: "task-2".into(),
            status: "failed".into(),
            pr_url: None,
            ci_status: CiStatus::Failed,
            review_outcome: ReviewOutcome::Skipped,
            review_rounds: 0,
            quality_signals: Vec::new(),
        };
        let json = serde_json::to_string(&proof).unwrap();
        assert!(!json.contains("quality_signals"));
        assert!(!json.contains("pr_url"));
    }

    #[test]
    fn with_quality_signals_replaces_vector() {
        let signal = QualitySignal {
            name: "complexity".into(),
            value: "low".into(),
        };
        let proof = ProofOfWork {
            task_id: "task-3".into(),
            status: "done".into(),
            pr_url: None,
            ci_status: CiStatus::Unknown,
            review_outcome: ReviewOutcome::Skipped,
            review_rounds: 0,
            quality_signals: Vec::new(),
        }
        .with_quality_signals(vec![signal.clone()]);
        assert_eq!(proof.quality_signals, vec![signal]);
    }
}
