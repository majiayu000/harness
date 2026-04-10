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
use std::fs::File;
use std::io::{BufRead, BufReader, Write as IoWrite};
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

/// Statistics collected during a [`replay_events`] pass.
#[derive(Debug, Default)]
pub struct ReplayStats {
    /// Number of non-empty lines processed.
    pub total_lines: usize,
    /// Number of lines that could not be parsed (corrupt or I/O error).
    pub corrupt_lines: usize,
    /// Number of distinct task IDs encountered.
    pub tasks_seen: usize,
}

/// Return value of [`replay_events`].
#[derive(Debug)]
pub struct ReplayResult {
    /// Per-task reconstructed state.
    pub states: HashMap<String, ReplayedState>,
    /// Parsing statistics from this replay pass.
    pub stats: ReplayStats,
}

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

/// Read `task-events.jsonl` line-by-line and return reconstructed state.
///
/// - Missing file → `Ok` with empty result (fresh install, no error).
/// - Unreadable file (permissions, I/O error) → `Err` propagated to caller.
/// - Corrupt lines → skipped with `warn!`; if ≥ 10 % of lines are corrupt,
///   an additional `error!` is emitted to surface possible log corruption.
pub fn replay_events(log_path: &Path) -> anyhow::Result<ReplayResult> {
    let file = match File::open(log_path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ReplayResult {
                states: HashMap::new(),
                stats: ReplayStats::default(),
            });
        }
        Err(e) => {
            return Err(anyhow::anyhow!(
                "event_replay: failed to open {}: {e}",
                log_path.display()
            ));
        }
    };

    // On macOS, File::open() succeeds on directories; detect this early to
    // avoid an infinite loop of EISDIR errors from BufReader::lines().
    let meta = file.metadata()?;
    if !meta.is_file() {
        return Err(anyhow::anyhow!(
            "event_replay: {} is not a regular file",
            log_path.display()
        ));
    }

    let mut states: HashMap<String, ReplayedState> = HashMap::new();
    let mut total_lines = 0usize;
    let mut corrupt_lines = 0usize;

    for line in BufReader::new(file).lines() {
        let line = match line {
            Ok(l) => l,
            Err(e) => {
                // Return Err rather than break with partial state: using an
                // incomplete event history is fail-open and can persist stale
                // values (e.g. a stale pr_url) into the database.
                // EISDIR is already caught above by the meta.is_file() check,
                // so this path only fires for genuine mid-stream I/O errors.
                return Err(anyhow::anyhow!(
                    "event_replay: I/O error reading {}: {e}",
                    log_path.display()
                ));
            }
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        total_lines += 1;
        match serde_json::from_str::<TaskEvent>(trimmed) {
            Ok(event) => apply_event(&mut states, event),
            Err(e) => {
                tracing::warn!(
                    "event_replay: skipping malformed line: {e} — line: {}",
                    &trimmed[..trimmed.len().min(80)]
                );
                corrupt_lines += 1;
            }
        }
    }

    // Threshold-based error: ≥ 10 % corrupt lines signals possible corruption.
    if corrupt_lines > 0 && total_lines > 0 && corrupt_lines * 10 >= total_lines {
        tracing::error!(
            corrupt_lines,
            total_lines,
            "event_replay: event log integrity degraded — possible corruption"
        );
    }

    let tasks_seen = states.len();
    Ok(ReplayResult {
        states,
        stats: ReplayStats {
            total_lines,
            corrupt_lines,
            tasks_seen,
        },
    })
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
    let result = replay_events(log_path)?;
    let states = result.states;
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
#[path = "event_replay_tests.rs"]
mod tests;
