use super::*;
use crate::task_runner::{RoundResult, TaskState};

fn task_with_id(id: &str) -> TaskState {
    TaskState::new(harness_core::types::TaskId(id.to_string()))
}

#[test]
fn proof_for_done_task_with_lgtm_is_passed_and_approved() {
    let mut task = task_with_id("done-lgtm");
    task.status = TaskStatus::Done;
    task.turn = 3;
    task.pr_url = Some("https://github.com/owner/repo/pull/7".to_string());
    task.rounds
        .push(RoundResult::new(1, "implement", "ok", None, None, None));
    task.rounds
        .push(RoundResult::new(2, "review", "needs_fix", None, None, None));
    task.rounds
        .push(RoundResult::new(3, "review", "lgtm", None, None, None));

    let proof = proof_from_state(&task);
    assert_eq!(proof.task_id, "done-lgtm");
    assert_eq!(proof.status, "done");
    assert_eq!(
        proof.pr_url.as_deref(),
        Some("https://github.com/owner/repo/pull/7")
    );
    assert_eq!(proof.ci_status, CiStatus::Passed);
    assert_eq!(proof.review_outcome, ReviewOutcome::Approved);
    assert_eq!(proof.review_rounds, 2);
    assert!(proof
        .quality_signals
        .iter()
        .any(|q| q.name == "turns" && q.value == "3"));
}

#[test]
fn proof_for_failed_task_records_failed_ci() {
    let mut task = task_with_id("failed");
    task.status = TaskStatus::Failed;
    task.error = Some("agent crashed".to_string());
    task.rounds.push(RoundResult::new(
        1,
        "review",
        "quota_exhausted",
        None,
        None,
        None,
    ));

    let proof = proof_from_state(&task);
    assert_eq!(proof.status, "failed");
    assert_eq!(proof.ci_status, CiStatus::Failed);
    assert_eq!(proof.review_outcome, ReviewOutcome::Skipped);
    assert_eq!(proof.review_rounds, 1);
    assert!(proof
        .quality_signals
        .iter()
        .any(|q| q.name == "error" && q.value == "agent crashed"));
}

#[test]
fn proof_for_done_without_review_is_unknown_and_skipped() {
    let mut task = task_with_id("done-no-review");
    task.status = TaskStatus::Done;
    task.rounds
        .push(RoundResult::new(1, "implement", "ok", None, None, None));

    let proof = proof_from_state(&task);
    assert_eq!(proof.ci_status, CiStatus::Unknown);
    assert_eq!(proof.review_outcome, ReviewOutcome::Skipped);
    assert_eq!(proof.review_rounds, 0);
}

#[test]
fn proof_marks_changes_requested_when_last_review_is_needs_fix() {
    let mut task = task_with_id("changes-requested");
    task.status = TaskStatus::Done;
    task.rounds
        .push(RoundResult::new(1, "review", "lgtm", None, None, None));
    task.rounds
        .push(RoundResult::new(2, "review", "needs_fix", None, None, None));

    let proof = proof_from_state(&task);
    assert_eq!(proof.review_outcome, ReviewOutcome::ChangesRequested);
    assert_eq!(proof.ci_status, CiStatus::Unknown);
}

#[test]
fn proof_does_not_treat_fallback_ready_to_merge_as_passed_ci() {
    let mut task = task_with_id("ready-to-merge");
    task.status = TaskStatus::Done;
    task.rounds.push(RoundResult::new(
        1,
        "review",
        "ready_to_merge",
        None,
        None,
        None,
    ));

    let proof = proof_from_state(&task);
    assert_eq!(proof.review_outcome, ReviewOutcome::Skipped);
    assert_eq!(proof.ci_status, CiStatus::Unknown);
}

#[test]
fn proof_counts_agent_review_approval_as_review_evidence() {
    let mut task = task_with_id("agent-review-approved");
    task.status = TaskStatus::Done;
    task.rounds.push(RoundResult::new(
        1,
        "agent_review",
        "approved",
        None,
        None,
        None,
    ));

    let proof = proof_from_state(&task);
    assert_eq!(proof.review_outcome, ReviewOutcome::Approved);
    assert_eq!(proof.ci_status, CiStatus::Passed);
    assert_eq!(proof.review_rounds, 1);
}

#[test]
fn proof_counts_agent_review_issues_as_changes_requested() {
    let mut task = task_with_id("agent-review-issues");
    task.status = TaskStatus::Done;
    task.rounds.push(RoundResult::new(
        1,
        "agent_review",
        "2 issues",
        None,
        None,
        None,
    ));

    let proof = proof_from_state(&task);
    assert_eq!(proof.review_outcome, ReviewOutcome::ChangesRequested);
    assert_eq!(proof.ci_status, CiStatus::Unknown);
    assert_eq!(proof.review_rounds, 1);
}
