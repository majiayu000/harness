use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use harness_core::proof_of_work::{
    CiStatus, ProofOfWork, QualitySignal, ReviewOutcome, ACTION_REVIEW, RESULT_LGTM,
};
use serde_json::json;
use std::sync::Arc;

use super::state::AppState;
use crate::task_runner::{RoundResult, TaskState, TaskStatus};

/// Derive a `ProofOfWork` from a `TaskState`.
///
/// Scans `rounds` in reverse to find the final review detail, counts review
/// rounds, and infers outcomes from known result label constants.
fn proof_from_state(state: &TaskState) -> ProofOfWork {
    let review_rounds_vec: Vec<&RoundResult> = state
        .rounds
        .iter()
        .filter(|r| r.action == ACTION_REVIEW)
        .collect();

    let review_rounds = review_rounds_vec.len() as u32;

    // LGTM if any review round was approved.
    let has_lgtm = review_rounds_vec.iter().any(|r| r.result == RESULT_LGTM);

    let review_outcome = if review_rounds == 0 {
        ReviewOutcome::Skipped
    } else if has_lgtm {
        ReviewOutcome::Approved
    } else {
        ReviewOutcome::ChangesRequested
    };

    // CI status: Passed only when approved and Done; Failed when task Failed.
    let ci_status = match &state.status {
        TaskStatus::Done if review_outcome == ReviewOutcome::Approved => CiStatus::Passed,
        // Graduated Done (rounds exhausted but few issues remain) — unknown CI.
        TaskStatus::Done => CiStatus::Unknown,
        TaskStatus::Failed => CiStatus::Failed,
        _ => CiStatus::Unknown,
    };

    // Detail from the last review round that has a detail field.
    let final_review_detail = state
        .rounds
        .iter()
        .rev()
        .find(|r| r.action == ACTION_REVIEW && r.detail.is_some())
        .and_then(|r| r.detail.clone());

    // Basic quality signals derived from the task.
    let mut signals: Vec<QualitySignal> = vec![
        QualitySignal {
            label: "review_rounds".to_string(),
            value: review_rounds.to_string(),
        },
        QualitySignal {
            label: "pr_url".to_string(),
            value: if state.pr_url.is_some() {
                "present".to_string()
            } else {
                "absent".to_string()
            },
        },
    ];
    if let Some(note) = &state.error {
        signals.push(QualitySignal {
            label: "completion_note".to_string(),
            value: note.clone(),
        });
    }

    ProofOfWork {
        task_id: state.id.0.clone(),
        pr_url: state.pr_url.clone(),
        repo: state.repo.clone(),
        issue: state.issue,
        ci_status,
        review_outcome,
        review_rounds,
        final_review_detail,
        quality_signals: signals,
    }
}

/// GET /tasks/{id}/proof — machine-readable proof-of-work for a completed task.
///
/// Returns 404 when the task does not exist, 422 when the task has not yet
/// reached the `Done` state.
pub(crate) async fn get_task_proof(
    State(state): State<Arc<AppState>>,
    Path(task_id): Path<String>,
) -> Response {
    let id = harness_core::types::TaskId(task_id);

    let task = match state.core.tasks.get_with_db_fallback(&id).await {
        Ok(Some(t)) => t,
        Ok(None) => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"error": "task not found"})),
            )
                .into_response()
        }
        Err(e) => {
            tracing::error!("get_task_proof: DB lookup failed for {id:?}: {e}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "internal server error"})),
            )
                .into_response();
        }
    };

    if task.status != TaskStatus::Done {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({
                "error": "proof-of-work is only available for completed tasks",
                "status": task.status.as_ref(),
            })),
        )
            .into_response();
    }

    let proof = proof_from_state(&task);
    Json(proof).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task_runner::{RoundResult, TaskState};

    fn make_state_with_rounds(status: TaskStatus, rounds: Vec<RoundResult>) -> TaskState {
        let mut s = TaskState::new(harness_core::types::TaskId("test-id".to_string()));
        s.status = status;
        s.rounds = rounds;
        s
    }

    fn review_round(result: &str, detail: Option<&str>) -> RoundResult {
        RoundResult {
            turn: 1,
            action: ACTION_REVIEW.to_string(),
            result: result.to_string(),
            detail: detail.map(|d| d.to_string()),
            first_token_latency_ms: None,
        }
    }

    #[test]
    fn from_task_done_with_approved_review() {
        let rounds = vec![
            review_round("fixed", None),
            review_round(RESULT_LGTM, Some("all good")),
        ];
        let state = make_state_with_rounds(TaskStatus::Done, rounds);
        let proof = proof_from_state(&state);

        assert_eq!(proof.review_outcome, ReviewOutcome::Approved);
        assert_eq!(proof.ci_status, CiStatus::Passed);
        assert_eq!(proof.review_rounds, 2);
        assert_eq!(proof.final_review_detail.as_deref(), Some("all good"));
    }

    #[test]
    fn from_task_done_ci_failed_no_lgtm() {
        let rounds = vec![review_round("fixed", None)];
        let state = make_state_with_rounds(TaskStatus::Failed, rounds);
        let proof = proof_from_state(&state);

        assert_eq!(proof.review_outcome, ReviewOutcome::ChangesRequested);
        assert_eq!(proof.ci_status, CiStatus::Failed);
    }

    #[test]
    fn from_task_no_review_rounds() {
        let state = make_state_with_rounds(TaskStatus::Done, vec![]);
        let proof = proof_from_state(&state);

        assert_eq!(proof.review_outcome, ReviewOutcome::Skipped);
        assert_eq!(proof.ci_status, CiStatus::Unknown);
        assert_eq!(proof.review_rounds, 0);
        assert!(proof.final_review_detail.is_none());
    }
}
