use super::*;
use harness_core::types::TaskId;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Record};
use tracing::subscriber::Interest;
use tracing::{Event, Id, Metadata, Subscriber};

#[derive(Clone, Default)]
struct ReplayCapturedSubscriber {
    output: Arc<Mutex<Vec<String>>>,
    next_span_id: Arc<AtomicU64>,
}

#[derive(Default)]
struct ReplayEventVisitor(String);

impl Visit for ReplayEventVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if !self.0.is_empty() {
            self.0.push(' ');
        }
        self.0.push_str(field.name());
        self.0.push('=');
        self.0.push_str(&format!("{value:?}"));
    }
}

impl Subscriber for ReplayCapturedSubscriber {
    fn register_callsite(&self, _metadata: &'static Metadata<'static>) -> Interest {
        Interest::always()
    }

    fn enabled(&self, _metadata: &Metadata<'_>) -> bool {
        true
    }

    fn new_span(&self, _attributes: &Attributes<'_>) -> Id {
        Id::from_u64(self.next_span_id.fetch_add(1, Ordering::Relaxed) + 1)
    }

    fn record(&self, _span: &Id, _values: &Record<'_>) {}

    fn record_follows_from(&self, _span: &Id, _follows: &Id) {}

    fn event(&self, event: &Event<'_>) {
        let mut visitor = ReplayEventVisitor::default();
        event.record(&mut visitor);
        match self.output.lock() {
            Ok(mut output) => output.push(visitor.0),
            Err(poisoned) => poisoned.into_inner().push(visitor.0),
        }
    }

    fn enter(&self, _span: &Id) {}

    fn exit(&self, _span: &Id) {}
}

impl ReplayCapturedSubscriber {
    fn output(&self) -> String {
        let events = match self.output.lock() {
            Ok(output) => output.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        };
        events.join("\n")
    }
}

fn ts() -> u64 {
    0
}

fn make_event_log(dir: &std::path::Path) -> TaskEventLog {
    TaskEventLog::open(&dir.join("task-events.jsonl")).unwrap()
}

fn postgres_tests_configured() -> bool {
    std::env::var("HARNESS_DATABASE_URL").is_ok_and(|url| !url.trim().is_empty())
}

// ── apply_event tests ─────────────────────────────────────────────────────

#[test]
fn apply_created_sets_no_status() {
    let mut states = HashMap::new();
    apply_event(
        &mut states,
        TaskEvent::Created {
            task_id: "t1".into(),
            ts: ts(),
        },
    );
    let s = &states["t1"];
    assert!(s.status.is_none());
    assert!(!s.terminal);
}

#[test]
fn apply_status_changed_updates_status_and_turn() {
    let mut states = HashMap::new();
    apply_event(
        &mut states,
        TaskEvent::StatusChanged {
            task_id: "t1".into(),
            ts: ts(),
            status: "implementing".into(),
            turn: 3,
        },
    );
    let s = &states["t1"];
    assert!(matches!(s.status, Some(TaskStatus::Implementing)));
    assert_eq!(s.turn, Some(3));
}

#[test]
fn apply_failed_sets_terminal() {
    let mut states = HashMap::new();
    apply_event(
        &mut states,
        TaskEvent::Failed {
            task_id: "t1".into(),
            ts: ts(),
            reason: "boom".into(),
        },
    );
    let s = &states["t1"];
    assert!(matches!(s.status, Some(TaskStatus::Failed)));
    assert!(s.terminal);
}

#[test]
fn apply_completed_sets_terminal() {
    let mut states = HashMap::new();
    apply_event(
        &mut states,
        TaskEvent::Completed {
            task_id: "t1".into(),
            ts: ts(),
        },
    );
    let s = &states["t1"];
    assert!(matches!(s.status, Some(TaskStatus::Done)));
    assert!(s.terminal);
}

#[test]
fn apply_cancelled_sets_terminal() {
    let mut states = HashMap::new();
    apply_event(
        &mut states,
        TaskEvent::Cancelled {
            task_id: "t1".into(),
            ts: ts(),
            reason: Some("operator_cancelled".into()),
        },
    );
    let s = &states["t1"];
    assert!(matches!(s.status, Some(TaskStatus::Cancelled)));
    assert!(s.terminal);
}

#[test]
fn apply_event_ignores_status_changed_after_terminal_failed() {
    let mut states = HashMap::new();
    apply_event(
        &mut states,
        TaskEvent::Failed {
            task_id: "t1".into(),
            ts: ts(),
            reason: "".into(),
        },
    );
    // Late StatusChanged must be ignored (stale-event resistance).
    apply_event(
        &mut states,
        TaskEvent::StatusChanged {
            task_id: "t1".into(),
            ts: ts(),
            status: "implementing".into(),
            turn: 5,
        },
    );
    let s = &states["t1"];
    assert!(matches!(s.status, Some(TaskStatus::Failed)));
    assert!(s.terminal);
}

#[test]
fn apply_pr_detected_sets_pr_url() {
    let mut states = HashMap::new();
    apply_event(
        &mut states,
        TaskEvent::PrDetected {
            task_id: "t1".into(),
            ts: ts(),
            pr_url: "https://github.com/o/r/pull/1".into(),
        },
    );
    assert_eq!(
        states["t1"].pr_url.as_deref(),
        Some("https://github.com/o/r/pull/1")
    );
}

#[test]
fn pr_url_from_pr_detected_not_overwritten_by_status_changed() {
    let mut states = HashMap::new();
    apply_event(
        &mut states,
        TaskEvent::PrDetected {
            task_id: "t1".into(),
            ts: ts(),
            pr_url: "https://github.com/o/r/pull/42".into(),
        },
    );
    apply_event(
        &mut states,
        TaskEvent::StatusChanged {
            task_id: "t1".into(),
            ts: ts(),
            status: "reviewing".into(),
            turn: 2,
        },
    );
    // pr_url must survive the status change.
    assert_eq!(
        states["t1"].pr_url.as_deref(),
        Some("https://github.com/o/r/pull/42")
    );
}

#[test]
fn apply_round_completed_increments_count() {
    let mut states = HashMap::new();
    apply_event(
        &mut states,
        TaskEvent::RoundCompleted {
            task_id: "t1".into(),
            ts: ts(),
            round: 1,
            result: "lgtm".into(),
        },
    );
    apply_event(
        &mut states,
        TaskEvent::RoundCompleted {
            task_id: "t1".into(),
            ts: ts(),
            round: 2,
            result: "fixed".into(),
        },
    );
    assert_eq!(states["t1"].rounds_count, 2);
}

// ── replay_events tests ───────────────────────────────────────────────────

#[test]
fn replay_events_returns_empty_for_missing_file() {
    let dir = tempfile::tempdir().unwrap();
    let result = replay_events(&dir.path().join("task-events.jsonl")).unwrap();
    assert!(result.states.is_empty());
}

#[test]
fn replay_events_skips_malformed_lines_and_continues() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("task-events.jsonl");
    std::fs::write(
        &path,
        concat!(
            "{\"type\":\"created\",\"task_id\":\"t1\",\"ts\":0}\n",
            "this is not json\n",
            "{\"type\":\"pr_detected\",\"task_id\":\"t1\",\"ts\":0,\"pr_url\":\"https://github.com/o/r/pull/9\"}\n",
        ),
    )
    .unwrap();
    let result = replay_events(&path).unwrap();
    assert!(result.states.contains_key("t1"));
    assert_eq!(
        result.states["t1"].pr_url.as_deref(),
        Some("https://github.com/o/r/pull/9")
    );
}

#[test]
fn replay_events_returns_empty_for_empty_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("task-events.jsonl");
    std::fs::write(&path, "").unwrap();
    let result = replay_events(&path).unwrap();
    assert!(result.states.is_empty());
}

// ── TaskEventLog tests ────────────────────────────────────────────────────

#[test]
fn event_log_append_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let log = make_event_log(dir.path());
    log.append(&TaskEvent::Created {
        task_id: "t1".into(),
        ts: 42,
    });
    log.append(&TaskEvent::PrDetected {
        task_id: "t1".into(),
        ts: 43,
        pr_url: "https://github.com/o/r/pull/7".into(),
    });
    drop(log);

    let result = replay_events(&dir.path().join("task-events.jsonl")).unwrap();
    assert!(result.states.contains_key("t1"));
    assert_eq!(
        result.states["t1"].pr_url.as_deref(),
        Some("https://github.com/o/r/pull/7")
    );
}

#[test]
fn event_log_deduplication_two_completed_events() {
    let dir = tempfile::tempdir().unwrap();
    let log = make_event_log(dir.path());
    // Emit Completed twice (simulates sites 4 and 7 both firing).
    log.append(&TaskEvent::Completed {
        task_id: "t1".into(),
        ts: 1,
    });
    log.append(&TaskEvent::Completed {
        task_id: "t1".into(),
        ts: 2,
    });
    drop(log);

    let path = dir.path().join("task-events.jsonl");
    let result = replay_events(&path).unwrap();
    // apply_event ignores the second Completed; state is still Done.
    assert!(matches!(result.states["t1"].status, Some(TaskStatus::Done)));
    assert!(result.states["t1"].terminal);
}

// ── PR A: streaming / edge-case tests ─────────────────────────────────────

#[test]
fn replay_events_large_file_correct_line_count() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("task-events.jsonl");

    // Write 50_000 Created events (~50 bytes each ≈ 2.5 MB; enough to
    // demonstrate streaming without allocating the full content at once).
    let mut content = String::with_capacity(50_000 * 60);
    for i in 0..50_000u32 {
        content.push_str(&format!(
            "{{\"type\":\"created\",\"task_id\":\"t{i}\",\"ts\":0}}\n"
        ));
    }
    std::fs::write(&path, &content).unwrap();

    let result = replay_events(&path).unwrap();
    assert_eq!(result.stats.total_lines, 50_000);
    assert_eq!(result.stats.corrupt_lines, 0);
    assert_eq!(result.stats.tasks_seen, 50_000);
}

#[test]
fn replay_events_handles_crlf_and_trailing_newline() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("task-events.jsonl");
    // CRLF line endings + trailing newline.
    let content = "{\"type\":\"created\",\"task_id\":\"t1\",\"ts\":0}\r\n\
                   {\"type\":\"completed\",\"task_id\":\"t1\",\"ts\":1}\r\n";
    std::fs::write(&path, content).unwrap();

    let result = replay_events(&path).unwrap();
    assert!(result.states.contains_key("t1"));
    assert!(result.states["t1"].terminal);
    assert_eq!(result.stats.corrupt_lines, 0);
}

// ── PR B: corruption threshold tests ──────────────────────────────────────

#[test]
fn replay_stats_zero_corrupt_lines() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("task-events.jsonl");
    std::fs::write(
        &path,
        "{\"type\":\"created\",\"task_id\":\"t1\",\"ts\":0}\n\
         {\"type\":\"completed\",\"task_id\":\"t1\",\"ts\":1}\n",
    )
    .unwrap();
    let result = replay_events(&path).unwrap();
    assert_eq!(result.stats.corrupt_lines, 0);
    assert_eq!(result.stats.total_lines, 2);
}

#[test]
fn replay_stats_corrupt_below_threshold() {
    // 1 corrupt out of 20 lines = 5 % — below 10 % threshold.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("task-events.jsonl");
    let mut content = String::new();
    for i in 0..19u32 {
        content.push_str(&format!(
            "{{\"type\":\"created\",\"task_id\":\"t{i}\",\"ts\":0}}\n"
        ));
    }
    content.push_str("not-json\n");
    std::fs::write(&path, &content).unwrap();

    let result = replay_events(&path).unwrap();
    assert_eq!(result.stats.corrupt_lines, 1);
    assert_eq!(result.stats.total_lines, 20);
    // corrupt_lines * 10 = 10 < 20 = total_lines → below threshold
    assert!(result.stats.corrupt_lines * 10 < result.stats.total_lines);
}

#[test]
fn replay_stats_corrupt_at_or_above_threshold() {
    // 3 corrupt out of 15 lines = 20 % — above 10 % threshold.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("task-events.jsonl");
    let mut content = String::new();
    for i in 0..12u32 {
        content.push_str(&format!(
            "{{\"type\":\"created\",\"task_id\":\"t{i}\",\"ts\":0}}\n"
        ));
    }
    content.push_str("bad1\nbad2\nbad3\n");
    std::fs::write(&path, &content).unwrap();

    let result = replay_events(&path).unwrap();
    assert_eq!(result.stats.corrupt_lines, 3);
    assert_eq!(result.stats.total_lines, 15);
    // corrupt_lines * 10 = 30 >= 15 = total_lines → at/above threshold
    assert!(result.stats.corrupt_lines * 10 >= result.stats.total_lines);
}

#[test]
fn replay_events_errors_on_unreadable_path() {
    // A path that exists as a directory cannot be opened as a file.
    let dir = tempfile::tempdir().unwrap();
    let result = replay_events(dir.path()); // dir, not a file
    assert!(result.is_err());
}

// ── CompactLock PID-reuse detection tests (Linux only) ────────────────────

#[cfg(target_os = "linux")]
#[test]
fn compact_lock_is_stale_when_start_time_mismatches() {
    // Simulate PID reuse: write a lock with the current PID but a start time
    // that cannot match (start time 1 is before any real process).
    let dir = tempfile::tempdir().unwrap();
    let lock_path = dir.path().join("task-events.jsonl.compact.lock");
    let pid = std::process::id();
    let real_start =
        CompactLock::read_proc_start_time(pid).expect("must be able to read our own start time");
    let wrong_start = real_start.wrapping_add(1);
    std::fs::write(&lock_path, format!("{pid}:{wrong_start}")).unwrap();

    let meta = std::fs::metadata(&lock_path).unwrap();
    assert!(
        CompactLock::is_stale(&lock_path, &meta),
        "lock with mismatched start time should be stale (PID reuse)"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn compact_lock_is_not_stale_for_live_process_with_correct_start_time() {
    let dir = tempfile::tempdir().unwrap();
    let lock_path = dir.path().join("task-events.jsonl.compact.lock");
    let pid = std::process::id();
    let start =
        CompactLock::read_proc_start_time(pid).expect("must be able to read our own start time");
    std::fs::write(&lock_path, format!("{pid}:{start}")).unwrap();

    let meta = std::fs::metadata(&lock_path).unwrap();
    assert!(
        !CompactLock::is_stale(&lock_path, &meta),
        "lock held by current live process with correct start time should not be stale"
    );
}

// ── Integration: full round-trip ──────────────────────────────────────────

#[tokio::test]
async fn replay_and_recover_integration() -> anyhow::Result<()> {
    let _db_guard = crate::test_helpers::acquire_db_state_guard().await;
    let dir = tempfile::tempdir()?;
    let db_path = dir.path().join("tasks.db");
    let db = TaskDb::open(&db_path).await?;

    // Insert a task in-progress (simulates crash mid-implement).
    let mut state = crate::task_runner::TaskState::new(TaskId("task-abc".into()));
    state.status = crate::task_runner::TaskStatus::Implementing;
    db.insert(&state).await?;

    // Write an event log showing a PR was detected.
    let log_path = dir.path().join("task-events.jsonl");
    let log = TaskEventLog::open(&log_path)?;
    log.append(&TaskEvent::Created {
        task_id: "task-abc".into(),
        ts: 1,
    });
    log.append(&TaskEvent::StatusChanged {
        task_id: "task-abc".into(),
        ts: 2,
        status: "implementing".into(),
        turn: 1,
    });
    log.append(&TaskEvent::PrDetected {
        task_id: "task-abc".into(),
        ts: 3,
        pr_url: "https://github.com/o/r/pull/99".into(),
    });
    drop(log);

    // Replay should write pr_url back to DB.
    let updated = replay_and_recover(&db, &log_path).await?;
    assert_eq!(updated, 1);

    // The task should now have pr_url set in DB.
    let tasks = db.list().await?;
    let task = tasks.iter().find(|t| t.id.0 == "task-abc").unwrap();
    assert_eq!(
        task.pr_url.as_deref(),
        Some("https://github.com/o/r/pull/99")
    );

    Ok(())
}

#[tokio::test]
async fn replay_skips_phantom_task_not_in_db() -> anyhow::Result<()> {
    let _db_guard = crate::test_helpers::acquire_db_state_guard().await;
    let dir = tempfile::tempdir()?;
    let db_path = dir.path().join("tasks.db");
    let db = TaskDb::open(&db_path).await?;

    let log_path = dir.path().join("task-events.jsonl");
    let log = TaskEventLog::open(&log_path)?;
    log.append(&TaskEvent::Created {
        task_id: "phantom-task".into(),
        ts: 1,
    });
    drop(log);

    // Should succeed without inserting a phantom row.
    let updated = replay_and_recover(&db, &log_path).await?;
    assert_eq!(updated, 0);

    let tasks = db.list().await?;
    assert!(tasks.is_empty());

    Ok(())
}

#[tokio::test]
async fn replay_event_log_has_pr_url_checkpoint_has_none() -> anyhow::Result<()> {
    let _db_guard = crate::test_helpers::acquire_db_state_guard().await;
    let dir = tempfile::tempdir()?;
    let db_path = dir.path().join("tasks.db");
    let db = TaskDb::open(&db_path).await?;

    let mut state = crate::task_runner::TaskState::new(TaskId("t-conflict".into()));
    state.status = crate::task_runner::TaskStatus::Implementing;
    db.insert(&state).await?;

    let log_path = dir.path().join("task-events.jsonl");
    let log = TaskEventLog::open(&log_path)?;
    log.append(&TaskEvent::PrDetected {
        task_id: "t-conflict".into(),
        ts: 1,
        pr_url: "https://github.com/o/r/pull/7".into(),
    });
    drop(log);

    replay_and_recover(&db, &log_path).await?;

    let tasks = db.list().await?;
    let t = tasks.iter().find(|t| t.id.0 == "t-conflict").unwrap();
    assert_eq!(t.pr_url.as_deref(), Some("https://github.com/o/r/pull/7"));

    Ok(())
}

#[tokio::test]
async fn replay_terminal_failed_overrides_implementing() -> anyhow::Result<()> {
    let _db_guard = crate::test_helpers::acquire_db_state_guard().await;
    let dir = tempfile::tempdir()?;
    let db_path = dir.path().join("tasks.db");
    let db = TaskDb::open(&db_path).await?;

    let mut state = crate::task_runner::TaskState::new(TaskId("t-term".into()));
    state.status = crate::task_runner::TaskStatus::Implementing;
    db.insert(&state).await?;

    let log_path = dir.path().join("task-events.jsonl");
    let log = TaskEventLog::open(&log_path)?;
    log.append(&TaskEvent::Failed {
        task_id: "t-term".into(),
        ts: 1,
        reason: "crashed".into(),
    });
    drop(log);

    replay_and_recover(&db, &log_path).await?;

    // recover_in_progress won't touch it since it's now 'failed'.
    let tasks = db.list().await?;
    let t = tasks.iter().find(|t| t.id.0 == "t-term").unwrap();
    assert!(matches!(t.status, TaskStatus::Failed));

    Ok(())
}

#[tokio::test]
async fn replay_caller_counts_and_logs_only_applied_writes() -> anyhow::Result<()> {
    if !postgres_tests_configured() {
        return Ok(());
    }
    let _db_guard = crate::test_helpers::acquire_db_state_guard().await;
    let dir = tempfile::tempdir()?;
    let db_path = dir.path().join("tasks.db");
    let db = TaskDb::open(&db_path).await?;

    let mut applied = crate::task_runner::TaskState::new(TaskId("replay-applied".into()));
    applied.status = TaskStatus::Implementing;
    db.insert(&applied).await?;

    let mut superseded = crate::task_runner::TaskState::new(TaskId("replay-superseded".into()));
    superseded.status = TaskStatus::Done;
    superseded.scheduler.mark_terminal(&TaskStatus::Done);
    db.insert(&superseded).await?;

    let log_path = dir.path().join("task-events.jsonl");
    let log = TaskEventLog::open(&log_path)?;
    for task_id in ["replay-applied", "replay-superseded"] {
        log.append(&TaskEvent::Completed {
            task_id: task_id.into(),
            ts: 1,
        });
    }
    drop(log);

    let captured = ReplayCapturedSubscriber::default();
    let updated = {
        let _subscriber_guard = tracing::subscriber::set_default(captured.clone());
        replay_and_recover(&db, &log_path).await?
    };
    assert_eq!(updated, 1, "only the durable replay write is counted");
    assert_eq!(
        db.get("replay-applied")
            .await?
            .expect("applied task")
            .status,
        TaskStatus::Done
    );
    assert_eq!(
        db.get("replay-superseded")
            .await?
            .expect("superseded task")
            .version,
        0,
        "superseded replay must not rewrite the task"
    );
    let output = captured.output();
    assert!(
        !output.contains("applied replayed state to 2 task(s)"),
        "replay aggregate wording must not count the superseded write: {output}"
    );
    assert!(
        output.contains("replayed state superseded by authoritative durable state"),
        "superseded replay must emit non-success diagnostic wording: {output}"
    );
    Ok(())
}

#[tokio::test]
async fn replay_conflict_preserves_terminal_log() -> anyhow::Result<()> {
    if !postgres_tests_configured() {
        return Ok(());
    }
    let _db_guard = crate::test_helpers::acquire_db_state_guard().await;
    let dir = tempfile::tempdir()?;
    let db_path = dir.path().join("tasks.db");
    let db = TaskDb::open(&db_path).await?;

    let mut state = crate::task_runner::TaskState::new(TaskId("replay-conflict".into()));
    state.status = TaskStatus::Failed;
    state.pr_url = Some("https://github.com/o/r/pull/999".to_string());
    db.insert(&state).await?;

    let log_path = dir.path().join("task-events.jsonl");
    let log = TaskEventLog::open(&log_path)?;
    log.append(&TaskEvent::PrDetected {
        task_id: "replay-conflict".into(),
        ts: 1,
        pr_url: "https://github.com/o/r/pull/1716".into(),
    });
    log.append(&TaskEvent::Completed {
        task_id: "replay-conflict".into(),
        ts: 2,
    });
    drop(log);
    let before = std::fs::read(&log_path)?;

    let captured = ReplayCapturedSubscriber::default();
    let error = {
        let _subscriber_guard = tracing::subscriber::set_default(captured.clone());
        replay_and_recover(&db, &log_path)
            .await
            .expect_err("contradictory terminal replay must fail closed")
    };
    assert!(
        error
            .downcast_ref::<crate::task_db::TaskRecoveryConflict>()
            .is_some(),
        "replay conflict should retain its typed error: {error:#}"
    );
    assert_eq!(
        std::fs::read(&log_path)?,
        before,
        "terminal replay evidence must remain byte-for-byte unchanged"
    );
    let output = captured.output();
    assert!(
        !output.contains("applied replayed state") && !output.contains("compacted log"),
        "conflicting replay must emit no aggregate success wording: {output}"
    );
    Ok(())
}

// Surface 4 regression guard: every TaskEvent variant serialises to JSONL
// containing only task_id, ts, and variant-specific scalar fields.
// No workspace path must ever appear in the wire format.
#[test]
fn surface4_task_event_jsonl_has_no_workspace_paths() {
    let events = [
        TaskEvent::Created {
            task_id: "t1".into(),
            ts: 0,
        },
        TaskEvent::StatusChanged {
            task_id: "t1".into(),
            ts: 1,
            status: "implementing".into(),
            turn: 1,
        },
        TaskEvent::Failed {
            task_id: "t1".into(),
            ts: 2,
            reason: "something went wrong".into(),
        },
        TaskEvent::Completed {
            task_id: "t1".into(),
            ts: 3,
        },
        TaskEvent::PrDetected {
            task_id: "t1".into(),
            ts: 4,
            pr_url: "https://github.com/o/r/pull/1".into(),
        },
        TaskEvent::RoundCompleted {
            task_id: "t1".into(),
            ts: 5,
            round: 1,
            result: "passed".into(),
        },
    ];
    for event in &events {
        let json = serde_json::to_string(event).expect("TaskEvent must serialise");
        assert!(
            !json.contains("/workspaces/"),
            "TaskEvent JSON must not contain a workspace path: {json}"
        );
    }
}
