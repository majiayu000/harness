//! Event sourcing for crash recovery.
//!
//! Writes a newline-delimited JSON (JSONL) stream of [`TaskEvent`]s to
//! `task-events.jsonl` alongside `tasks.db`.  On server restart,
//! [`replay_and_recover`] replays the log and writes the reconstructed state
//! back to the database **before** the legacy `recover_in_progress()` fallback
//! runs, so event-sourced data wins over checkpoint data.
//!
//! Precedence rule (explicit): event_log > checkpoint > db_snapshot.

use crate::task_db::TaskDb;
use crate::task_runner::TaskStatus;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::Write as IoWrite;
use std::path::Path;
use std::str::FromStr;
use std::sync::Mutex;

// ──────────────────────────────────────────────────────────────────────────────
// TaskEvent wire format
// ──────────────────────────────────────────────────────────────────────────────

/// A single event appended to `task-events.jsonl`.
///
/// Each variant carries `task_id` and `ts` (Unix seconds) as the minimum
/// durable state required for crash recovery.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TaskEvent {
    /// Task row was inserted into the database for the first time.
    Created { task_id: String, ts: u64 },
    /// Task status or turn counter changed (non-terminal).
    StatusChanged {
        task_id: String,
        ts: u64,
        /// String form of [`TaskStatus`] (e.g. `"implementing"`).
        status: String,
        turn: u32,
    },
    /// Task reached terminal `Failed` state.
    Failed {
        task_id: String,
        ts: u64,
        reason: String,
    },
    /// Task reached terminal `Done` state.
    Completed { task_id: String, ts: u64 },
    /// A pull-request URL was extracted from the agent's output.
    PrDetected {
        task_id: String,
        ts: u64,
        pr_url: String,
    },
    /// One review/eval round completed.
    RoundCompleted {
        task_id: String,
        ts: u64,
        round: u32,
        result: String,
    },
}

impl TaskEvent {
    /// Return the `task_id` shared by all variants.
    pub fn task_id(&self) -> &str {
        match self {
            TaskEvent::Created { task_id, .. }
            | TaskEvent::StatusChanged { task_id, .. }
            | TaskEvent::Failed { task_id, .. }
            | TaskEvent::Completed { task_id, .. }
            | TaskEvent::PrDetected { task_id, .. }
            | TaskEvent::RoundCompleted { task_id, .. } => task_id,
        }
    }
}

/// Return the current Unix timestamp in seconds.
pub(crate) fn now_ts() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

// ──────────────────────────────────────────────────────────────────────────────
// TaskEventLog — append-only JSONL writer
// ──────────────────────────────────────────────────────────────────────────────

/// Append-only writer for the `task-events.jsonl` file.
///
/// Thread-safe via an internal `Mutex<File>`.  `append()` is infallible from
/// the caller's perspective: I/O errors are logged and discarded so that a
/// transient write failure never aborts the task being executed.
pub struct TaskEventLog {
    file: Mutex<std::fs::File>,
    path: std::path::PathBuf,
}

impl std::fmt::Debug for TaskEventLog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TaskEventLog")
            .field("path", &self.path)
            .finish()
    }
}

impl TaskEventLog {
    /// Open (or create) the JSONL log at `path`.
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        Ok(Self {
            file: Mutex::new(file),
            path: path.to_path_buf(),
        })
    }

    /// Append one event as a JSONL line.  Best-effort: I/O errors are logged
    /// and discarded so the caller never has to handle write failures.
    pub fn append(&self, event: &TaskEvent) {
        let line = match serde_json::to_string(event) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(
                    path = %self.path.display(),
                    "event_log: failed to serialize event: {e}"
                );
                return;
            }
        };
        let mut file = self.file.lock().unwrap();
        if let Err(e) = writeln!(file, "{line}") {
            tracing::warn!(
                path = %self.path.display(),
                "event_log: failed to append event: {e}"
            );
        }
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Replay state machine
// ──────────────────────────────────────────────────────────────────────────────

/// Per-task state reconstructed from replaying the event log.
#[derive(Debug, Default)]
pub struct ReplayedState {
    pub status: Option<TaskStatus>,
    pub turn: Option<u32>,
    pub pr_url: Option<String>,
    pub rounds_count: u32,
    /// Set to `true` once a `Failed` or `Completed` event is applied.
    /// Subsequent terminal events for the same task are no-ops.
    pub terminal: bool,
}

/// Apply a single event to the running per-task state map.
///
/// Events that arrive after a terminal state is already recorded are silently
/// ignored — this covers double-emission from sites 3/4 (runner) vs 7/8
/// (executor).
pub fn apply_event(states: &mut HashMap<String, ReplayedState>, event: TaskEvent) {
    let task_id = event.task_id().to_string();
    let state = states.entry(task_id).or_default();

    if state.terminal {
        return;
    }

    match event {
        TaskEvent::Created { .. } => {
            // Task was inserted — no additional state to reconstruct.
        }
        TaskEvent::StatusChanged { status, turn, .. } => {
            if let Ok(s) = TaskStatus::from_str(&status) {
                state.status = Some(s);
            }
            state.turn = Some(turn);
        }
        TaskEvent::Failed { .. } => {
            state.status = Some(TaskStatus::Failed);
            state.terminal = true;
        }
        TaskEvent::Completed { .. } => {
            state.status = Some(TaskStatus::Done);
            state.terminal = true;
        }
        TaskEvent::PrDetected { pr_url, .. } => {
            // PrDetected wins over any earlier value; never overwrite with a
            // later StatusChanged that does not carry pr_url.
            state.pr_url = Some(pr_url);
        }
        TaskEvent::RoundCompleted { .. } => {
            state.rounds_count += 1;
        }
    }
}

/// Read `task-events.jsonl` and return a map of task_id → [`ReplayedState`].
///
/// - Missing file → empty map (fresh install, no error).
/// - Corrupt lines → skipped with a warning (partial-write tolerance).
pub fn replay_events(log_path: &Path) -> HashMap<String, ReplayedState> {
    let data = match std::fs::read_to_string(log_path) {
        Ok(d) => d,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return HashMap::new(),
        Err(e) => {
            tracing::warn!("event_replay: failed to read {}: {e}", log_path.display());
            return HashMap::new();
        }
    };

    let mut states: HashMap<String, ReplayedState> = HashMap::new();
    for line in data.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        match serde_json::from_str::<TaskEvent>(trimmed) {
            Ok(event) => apply_event(&mut states, event),
            Err(e) => {
                tracing::warn!(
                    "event_replay: skipping malformed line: {e} — line: {}",
                    &trimmed[..trimmed.len().min(80)]
                );
            }
        }
    }
    states
}

// ──────────────────────────────────────────────────────────────────────────────
// Startup integration
// ──────────────────────────────────────────────────────────────────────────────

/// Replay the event log and apply reconstructed state to the database.
///
/// Must be called **before** `recover_in_progress()` so that:
/// 1. Tasks whose only copy of `pr_url` lives in the event log get it written
///    to the `tasks` table, allowing `recover_in_progress()` to resume them.
/// 2. Tasks that completed/failed between the last DB write and the crash are
///    marked terminal, so `recover_in_progress()` leaves them alone.
///
/// Returns the number of tasks whose DB rows were updated.
pub async fn replay_and_recover(db: &TaskDb, log_path: &Path) -> anyhow::Result<u32> {
    let states = replay_events(log_path);
    if states.is_empty() {
        return Ok(0);
    }

    let mut updated = 0u32;

    for (task_id_str, replayed) in &states {
        // Skip tasks unknown to the DB — do not insert phantom rows.
        if !db.exists_by_id(task_id_str).await? {
            tracing::debug!(
                task_id = task_id_str,
                "event_replay: task not found in DB, skipping"
            );
            continue;
        }

        let terminal_status = if replayed.terminal {
            replayed.status.as_ref().map(|s| s.as_ref().to_string())
        } else {
            None
        };

        db.apply_replayed_state(
            task_id_str,
            replayed.pr_url.as_deref(),
            terminal_status.as_deref(),
        )
        .await?;

        updated += 1;
    }

    if updated > 0 {
        tracing::info!("event_replay: applied replayed state to {updated} task(s)");
    }

    Ok(updated)
}

// ──────────────────────────────────────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use harness_core::types::TaskId;

    fn ts() -> u64 {
        0
    }

    fn make_event_log(dir: &std::path::Path) -> TaskEventLog {
        TaskEventLog::open(&dir.join("task-events.jsonl")).unwrap()
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
        let result = replay_events(&dir.path().join("task-events.jsonl"));
        assert!(result.is_empty());
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
        let result = replay_events(&path);
        assert!(result.contains_key("t1"));
        assert_eq!(
            result["t1"].pr_url.as_deref(),
            Some("https://github.com/o/r/pull/9")
        );
    }

    #[test]
    fn replay_events_returns_empty_for_empty_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("task-events.jsonl");
        std::fs::write(&path, "").unwrap();
        let result = replay_events(&path);
        assert!(result.is_empty());
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

        let result = replay_events(&dir.path().join("task-events.jsonl"));
        assert!(result.contains_key("t1"));
        assert_eq!(
            result["t1"].pr_url.as_deref(),
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
        let result = replay_events(&path);
        // apply_event ignores the second Completed; state is still Done.
        assert!(matches!(result["t1"].status, Some(TaskStatus::Done)));
        assert!(result["t1"].terminal);
    }

    // ── Integration: full round-trip ──────────────────────────────────────────

    #[tokio::test]
    async fn replay_and_recover_integration() -> anyhow::Result<()> {
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
}
