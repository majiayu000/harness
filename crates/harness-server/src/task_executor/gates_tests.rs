use super::*;
use crate::task_runner::TaskState;

#[test]
fn review_entry_decision_blocks_review_on_rebase_conflict() {
    assert_eq!(
        review_entry_decision(
            42,
            Some(&prompts::PrReviewPrepOutcome::RebaseConflict {
                paths: vec!["src/lib.rs".to_string(), "src/main.rs".to_string()],
            })
        ),
        ReviewEntryDecision::FailConflict {
            error:
                "PR #42 has rebase conflicts: src/lib.rs, src/main.rs; manual resolution required"
                    .to_string(),
            paths_csv: "src/lib.rs, src/main.rs".to_string(),
        }
    );
}

#[test]
fn review_entry_decision_propagates_rebase_pushed_flag() {
    assert_eq!(
        review_entry_decision(42, Some(&prompts::PrReviewPrepOutcome::RebasePushed)),
        ReviewEntryDecision::Proceed {
            rebase_pushed: true
        }
    );
    assert_eq!(
        review_entry_decision(42, Some(&prompts::PrReviewPrepOutcome::RebaseSkipped)),
        ReviewEntryDecision::Proceed {
            rebase_pushed: false
        }
    );
}

#[test]
fn review_prep_from_rounds_prefers_latest_implement_round() {
    let rounds = vec![
        RoundResult::new(1, "implement", "rebase_skipped", None, None, None),
        RoundResult::new(2, "review", "needs_fix", None, None, None),
        RoundResult::new(3, "implement", "rebase_pushed", None, None, None),
    ];
    assert_eq!(
        review_prep_from_rounds(&rounds),
        Some(prompts::PrReviewPrepOutcome::RebasePushed)
    );
}

#[test]
fn review_prep_from_rounds_recovers_rebase_conflict_detail() {
    let rounds = vec![RoundResult::new(
        1,
        "implement",
        "rebase_conflict",
        Some("REBASE_CONFLICT paths=src/lib.rs,src/main.rs".to_string()),
        None,
        None,
    )];
    assert_eq!(
        review_prep_from_rounds(&rounds),
        Some(prompts::PrReviewPrepOutcome::RebaseConflict {
            paths: vec!["src/lib.rs".to_string(), "src/main.rs".to_string()],
        })
    );
}

#[test]
fn review_prep_from_checkpoint_phase_recovers_rebase_state() {
    assert_eq!(
        review_prep_from_checkpoint_phase("rebase_pushed"),
        Some(prompts::PrReviewPrepOutcome::RebasePushed)
    );
    assert_eq!(
        review_prep_from_checkpoint_phase("rebase_skipped"),
        Some(prompts::PrReviewPrepOutcome::RebaseSkipped)
    );
    assert_eq!(
        review_prep_from_checkpoint_phase("rebase_conflict"),
        Some(prompts::PrReviewPrepOutcome::RebaseConflict { paths: Vec::new() })
    );
    assert_eq!(review_prep_from_checkpoint_phase("pr_created"), None);
}

#[tokio::test]
async fn fail_rebase_conflict_marks_task_failed_and_logs_event() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let store = TaskStore::open(&dir.path().join("tasks.db")).await?;
    let events = Arc::new(harness_observe::event_store::EventStore::new(dir.path()).await?);
    let task_id = TaskId::new();
    let state = TaskState::new(task_id.clone());
    store.insert(&state).await;

    fail_rebase_conflict(
        &store,
        &task_id,
        &events,
        42,
        "PR #42 has rebase conflicts: src/lib.rs, src/main.rs; manual resolution required"
            .to_string(),
        "src/lib.rs, src/main.rs".to_string(),
    )
    .await?;

    let final_state = store
        .get(&task_id)
        .ok_or_else(|| anyhow::anyhow!("task must exist"))?;
    assert!(matches!(final_state.status, TaskStatus::Failed));
    assert_eq!(
        final_state.error.as_deref(),
        Some("PR #42 has rebase conflicts: src/lib.rs, src/main.rs; manual resolution required")
    );

    let logged = events
        .query(&harness_core::types::EventFilters {
            hook: Some("pr_rebase_conflict".to_string()),
            ..harness_core::types::EventFilters::default()
        })
        .await?;
    assert_eq!(logged.len(), 1);
    assert_eq!(logged[0].reason.as_deref(), Some("src/lib.rs, src/main.rs"));
    let expected_detail = format!("task_id={} pr=42", task_id.as_str());
    assert_eq!(logged[0].detail.as_deref(), Some(expected_detail.as_str()));
    Ok(())
}

#[test]
fn manual_resolution_constant_matches_poller_substring() {
    // The intake/github_issues.rs poller checks `error.contains(MANUAL_RESOLUTION_REQUIRED)`
    // to decide whether to keep an issue in `dispatched`. A wording change to the
    // constant must be coordinated with that poller — this test pins the spelling.
    assert_eq!(MANUAL_RESOLUTION_REQUIRED, "manual resolution required");
}
