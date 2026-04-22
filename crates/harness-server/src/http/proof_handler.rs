use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use harness_core::proof_of_work::{
    CiStatus, ProofOfWork, QualitySignal, ReviewOutcome, ACTION_AGENT_REVIEW, ACTION_REVIEW,
    RESULT_APPROVED, RESULT_COMPLETED, RESULT_LGTM, RESULT_QUOTA_EXHAUSTED,
};
use serde_json::json;
use std::sync::Arc;

use super::state::AppState;
use crate::task_runner::{TaskState, TaskStatus};

/// Derive a `ProofOfWork` from a `TaskState`.
///
/// Scans `rounds` in reverse to find the final review detail, counts review
/// rounds, and infers outcomes from known result label constants.
fn proof_from_state(state: &TaskState) -> ProofOfWork {
    let mut review_rounds = 0u32;
    let mut has_lgtm = false;
    let mut saw_executed_review = false;
    let mut review_only = true;
    let mut final_review_detail = None;

    for round in &state.rounds {
        if round.action != ACTION_REVIEW && round.action != ACTION_AGENT_REVIEW {
            continue;
        }

        final_review_detail = round.detail.clone();

        if round.result == RESULT_QUOTA_EXHAUSTED {
            continue;
        }

        review_rounds += 1;
        saw_executed_review = true;
        review_only &= round.result == RESULT_COMPLETED;
        has_lgtm |= round.result == RESULT_LGTM
            || (round.action == ACTION_AGENT_REVIEW && round.result == RESULT_APPROVED);
    }

    let is_review_only = saw_executed_review && review_only;
    let quota_heuristic_done = state.status == TaskStatus::Done
        && state
            .error
            .as_deref()
            .is_some_and(|note| note.starts_with("LGTM via quota-heuristic:"));

    let review_outcome = if is_review_only {
        ReviewOutcome::NotApplicable
    } else if quota_heuristic_done || has_lgtm {
        ReviewOutcome::Approved
    } else if review_rounds == 0 {
        ReviewOutcome::Skipped
    } else {
        ReviewOutcome::ChangesRequested
    };

    // CI status: Passed only when approved and Done; Failed when task Failed.
    let ci_status = match &state.status {
        TaskStatus::Done if review_outcome == ReviewOutcome::Approved => CiStatus::Passed,
        TaskStatus::Done => CiStatus::Unknown,
        TaskStatus::Failed => CiStatus::Failed,
        _ => CiStatus::Unknown,
    };
    let mut signals: Vec<QualitySignal> = Vec::new();
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
        issue: state
            .issue
            .or_else(|| infer_issue_from_external_id(state.external_id.as_deref())),
        ci_status,
        review_outcome,
        review_rounds,
        final_review_detail,
        quality_signals: signals,
    }
}

fn infer_issue_from_external_id(external_id: Option<&str>) -> Option<u64> {
    let raw = match external_id {
        Some(raw) if raw.starts_with("issue:") => raw.strip_prefix("issue:")?,
        Some(raw) if raw.chars().all(|ch| ch.is_ascii_digit()) => raw,
        _ => return None,
    };

    raw.parse().ok()
}

/// GET /tasks/{id}/proof — machine-readable proof-of-work for a completed task.
///
/// Returns 404 when the task does not exist, 422 when the task has not yet
/// reached a terminal state.
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

    if !matches!(task.status, TaskStatus::Done | TaskStatus::Failed) {
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
#[path = "proof_handler_tests.rs"]
mod tests;
