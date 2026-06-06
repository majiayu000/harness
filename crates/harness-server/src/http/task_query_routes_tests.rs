use super::*;
use crate::task_runner::{RoundResult, TaskState};

fn task_with_id(id: &str) -> TaskState {
    TaskState::new(harness_core::types::TaskId(id.to_string()))
}

fn summary_with_created_at(id: &str, created_at: &str) -> TaskSummary {
    let mut task = task_with_id(id);
    task.created_at = Some(created_at.to_string());
    task.summary()
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

#[test]
fn runtime_proof_reports_cancelled_ci_as_unknown() {
    let workflow = harness_workflow::runtime::WorkflowInstance::new(
        harness_workflow::runtime::GITHUB_ISSUE_PR_DEFINITION_ID,
        1,
        "cancelled",
        harness_workflow::runtime::WorkflowSubject::new("issue", "issue:1139"),
    )
    .with_id("runtime-cancelled-proof")
    .with_data(json!({
        "task_id": "runtime-cancelled-proof-task",
        "pr_url": "https://github.com/owner/repo/pull/1139",
    }));

    let task_id = harness_core::types::TaskId("runtime-cancelled-proof-task".to_string());
    let proof = proof_from_runtime_workflow(&task_id, &workflow, &[], &[]);

    assert_eq!(proof.status, "cancelled");
    assert_eq!(proof.ci_status, CiStatus::Unknown);
    assert_eq!(proof.review_outcome, ReviewOutcome::Skipped);
}

#[test]
fn runtime_proof_excludes_merge_events_from_review_rounds() {
    let workflow = harness_workflow::runtime::WorkflowInstance::new(
        harness_workflow::runtime::GITHUB_ISSUE_PR_DEFINITION_ID,
        1,
        "done",
        harness_workflow::runtime::WorkflowSubject::new("issue", "issue:1140"),
    )
    .with_id("runtime-merged-proof")
    .with_data(json!({
        "task_id": "runtime-merged-proof-task",
        "pr_url": "https://github.com/owner/repo/pull/1140",
    }));
    let events = vec![
        harness_workflow::runtime::WorkflowEvent::new(
            workflow.id.clone(),
            1,
            "PrReadyToMerge",
            "test",
        ),
        harness_workflow::runtime::WorkflowEvent::new(
            workflow.id.clone(),
            2,
            "MergeApproved",
            "test",
        ),
        harness_workflow::runtime::WorkflowEvent::new(workflow.id.clone(), 3, "PrMerged", "test"),
    ];
    let decisions = vec![
        harness_workflow::runtime::WorkflowDecisionRecord::accepted(
            harness_workflow::runtime::WorkflowDecision::new(
                workflow.id.clone(),
                "awaiting_feedback",
                "mark_ready_to_merge",
                "ready_to_merge",
                "review passed",
            ),
            Some(events[0].id.clone()),
        ),
        harness_workflow::runtime::WorkflowDecisionRecord::accepted(
            harness_workflow::runtime::WorkflowDecision::new(
                workflow.id.clone(),
                "ready_to_merge",
                "approve_merge",
                "merging",
                "operator approved merge",
            ),
            Some(events[1].id.clone()),
        ),
        harness_workflow::runtime::WorkflowDecisionRecord::accepted(
            harness_workflow::runtime::WorkflowDecision::new(
                workflow.id.clone(),
                "merging",
                "record_pr_merged",
                "done",
                "PR merged",
            ),
            Some(events[2].id.clone()),
        ),
    ];

    let task_id = harness_core::types::TaskId("runtime-merged-proof-task".to_string());
    let proof = proof_from_runtime_workflow(&task_id, &workflow, &events, &decisions);

    assert_eq!(proof.review_outcome, ReviewOutcome::Approved);
    assert_eq!(proof.ci_status, CiStatus::Passed);
    assert_eq!(proof.review_rounds, 1);
}

#[test]
fn runtime_workflow_scheduler_state_only_marks_executing_states_running() {
    let blocked = runtime_workflow_scheduler_state("blocked", &TaskStatus::Waiting);
    assert_eq!(blocked.authority_state, SchedulerAuthorityState::Queued);

    let awaiting_feedback =
        runtime_workflow_scheduler_state("awaiting_feedback", &TaskStatus::Waiting);
    assert_eq!(
        awaiting_feedback.authority_state,
        SchedulerAuthorityState::Queued
    );

    let implementing = runtime_workflow_scheduler_state("implementing", &TaskStatus::Implementing);
    assert_eq!(
        implementing.authority_state,
        SchedulerAuthorityState::Running
    );

    let planning = runtime_workflow_scheduler_state("planning", &TaskStatus::Planning);
    assert_eq!(planning.authority_state, SchedulerAuthorityState::Running);
}

#[test]
fn paginate_task_summaries_returns_next_cursor_and_resumes_after_it() {
    let mut summaries = vec![
        summary_with_created_at("task-a", "2026-05-20T01:00:00Z"),
        summary_with_created_at("task-c", "2026-05-20T03:00:00Z"),
        summary_with_created_at("task-b", "2026-05-20T02:00:00Z"),
    ];
    sort_task_summaries(&mut summaries);
    let first_query = TaskListQuery {
        filter: TaskSummaryFilter::default(),
        limit: 2,
        cursor: None,
    };

    let (first_page, first_metadata) = paginate_task_summaries(&summaries, &first_query);

    assert_eq!(
        first_page
            .iter()
            .map(|summary| summary.id.as_str())
            .collect::<Vec<_>>(),
        vec!["task-c", "task-b"]
    );
    assert!(first_metadata.has_more);
    assert_eq!(
        first_metadata.next_cursor.as_deref(),
        Some("2026-05-20T02:00:00Z|task-b")
    );

    let second_query = TaskListQuery {
        filter: TaskSummaryFilter::default(),
        limit: 2,
        cursor: first_metadata
            .next_cursor
            .as_deref()
            .map(parse_task_list_cursor)
            .transpose()
            .expect("cursor should parse"),
    };
    let (second_page, second_metadata) = paginate_task_summaries(&summaries, &second_query);

    assert_eq!(
        second_page
            .iter()
            .map(|summary| summary.id.as_str())
            .collect::<Vec<_>>(),
        vec!["task-a"]
    );
    assert!(!second_metadata.has_more);
    assert_eq!(second_metadata.next_cursor, None);
}

#[test]
fn task_list_counts_use_returned_page_only() {
    let summaries = vec![
        summary_with_created_at("task-c", "2026-05-20T03:00:00Z"),
        summary_with_created_at("task-b", "2026-05-20T02:00:00Z"),
        summary_with_created_at("task-a", "2026-05-20T01:00:00Z"),
    ];
    let query = TaskListQuery {
        filter: TaskSummaryFilter::default(),
        limit: 2,
        cursor: None,
    };

    let (page, metadata) = paginate_task_summaries(&summaries, &query);
    let counts = task_list_counts(&page);

    assert!(metadata.has_more);
    assert_eq!(page.len(), 2);
    assert_eq!(counts.total, 2);
}

#[test]
fn sort_task_summaries_compares_rfc3339_instants() {
    let mut summaries = vec![
        summary_with_created_at("older-whole-second", "2026-05-20T01:00:00Z"),
        summary_with_created_at("newer-fractional", "2026-05-20T01:00:00.100Z"),
    ];

    sort_task_summaries(&mut summaries);

    assert_eq!(summaries[0].id.as_str(), "newer-fractional");
    assert_eq!(summaries[1].id.as_str(), "older-whole-second");
}
