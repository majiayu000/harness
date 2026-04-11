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
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader, Seek, SeekFrom, Write as IoWrite};
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

        // On Unix, `rename()` is atomic but leaves existing file descriptors
        // pointing to the old (now unlinked) inode.  When compaction replaces
        // `task-events.jsonl`, a running appender would silently write events
        // to the unlinked inode — events are lost once the fd is closed.
        // Detect the replacement by comparing inodes and reopen if needed.
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let replaced = file
                .metadata()
                .ok()
                .zip(std::fs::metadata(&self.path).ok())
                .map(|(fd_meta, path_meta)| fd_meta.ino() != path_meta.ino())
                .unwrap_or(false);
            if replaced {
                match std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&self.path)
                {
                    Ok(new_file) => {
                        tracing::debug!(
                            path = %self.path.display(),
                            "event_log: log file replaced by compaction; reopened"
                        );
                        *file = new_file;
                    }
                    Err(e) => {
                        tracing::warn!(
                            path = %self.path.display(),
                            "event_log: log file replaced but reopen failed: {e}"
                        );
                    }
                }
            }
        }

        if let Err(e) = writeln!(*file, "{line}") {
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

/// Compact `task-events.jsonl` by removing all events for tasks that have
/// reached a terminal state.
///
/// Terminal-task events are redundant after [`replay_and_recover`] has
/// committed their state to the database.  Removing them prevents the log from
/// growing unboundedly across server restarts.
///
/// Uses an atomic rename (write to a per-process `.tmp` file, then `rename`)
/// so that a crash during compaction leaves the original log intact.  A
/// [`CompactLock`] serialises concurrent server instances that share the same
/// data directory, preventing silent event loss from a rename race.
///
/// Returns the number of event lines removed.
fn compact_log(log_path: &Path, states: &HashMap<String, ReplayedState>) -> anyhow::Result<u32> {
    let terminal_ids: HashSet<&str> = states
        .iter()
        .filter(|(_, s)| s.terminal)
        .map(|(id, _)| id.as_str())
        .collect();

    if terminal_ids.is_empty() {
        return Ok(0);
    }

    // Acquire an exclusive lock to prevent concurrent server instances from
    // racing on the same log file.
    let lock_path = {
        let stem = log_path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        let mut p = log_path.to_path_buf();
        p.set_file_name(format!("{stem}.compact.lock"));
        p
    };
    let _lock = match CompactLock::try_acquire(&lock_path) {
        Ok(Some(lock)) => lock,
        Ok(None) => {
            tracing::warn!(
                path = %log_path.display(),
                "event_replay: compaction lock held by another process, skipping"
            );
            return Ok(0);
        }
        Err(e) => return Err(e),
    };

    // Unique tmp path per process (PID + sub-second nonce) so two instances
    // that somehow both pass the lock check cannot clobber each other's tmp.
    let tmp_path = {
        let pid = std::process::id();
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .subsec_nanos();
        let stem = log_path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        let mut p = log_path.to_path_buf();
        p.set_file_name(format!("{stem}.{pid}.{nonce}.tmp"));
        p
    };

    let src_file = std::fs::File::open(log_path)?;
    // Record the file size at open time.  After the filter pass, we drain any
    // bytes appended by concurrent writers between now and the rename below,
    // so events written during the compaction window are not silently lost.
    let snapshot_len = src_file.metadata()?.len();
    let reader = BufReader::new(src_file);
    let mut removed = 0u32;

    // Stream directly to the tmp file line-by-line — never buffer all lines
    // in memory, which could OOM the server on a large log.
    {
        let mut tmp_file = std::fs::File::create(&tmp_path)?;
        for line_result in reader.lines() {
            let line = line_result?;
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let keep = match serde_json::from_str::<TaskEvent>(trimmed) {
                Ok(event) => !terminal_ids.contains(event.task_id()),
                // Keep lines we cannot parse — do not silently drop unknown data.
                Err(_) => true,
            };
            if keep {
                writeln!(tmp_file, "{trimmed}")?;
            } else {
                removed += 1;
            }
        }

        // Drain bytes appended to the source file while the filter pass was
        // running.  A concurrent appender in another process may have written
        // events to the original inode after we opened it (snapshot_len).
        // Copy those bytes verbatim into the tmp file before the rename so
        // they are preserved.  The tiny window between this drain and the
        // rename is handled by the inode-detection reopen logic in
        // TaskEventLog::append, which redirects the next write to the new
        // inode if it detects the path has been replaced.
        {
            let mut drain = std::fs::File::open(log_path)?;
            drain.seek(SeekFrom::Start(snapshot_len))?;
            std::io::copy(&mut drain, &mut tmp_file)?;
        }

        // Flush buffered writer state then sync to durable storage before the
        // rename.  This ensures the compacted data survives a power failure
        // that occurs between flush and rename.
        tmp_file.flush()?;
        tmp_file.sync_all()?;
        // File is closed (dropped) here, before rename.
    }

    std::fs::rename(&tmp_path, log_path)?;

    // Sync the parent directory to make the directory-entry rename durable.
    if let Some(parent) = log_path.parent() {
        if let Ok(dir) = std::fs::File::open(parent) {
            if let Err(e) = dir.sync_all() {
                tracing::warn!(
                    path = %log_path.display(),
                    "event_replay: failed to fsync parent dir after compaction: {e}"
                );
            }
        }
    }

    Ok(removed)
}

// ──────────────────────────────────────────────────────────────────────────────
// CompactLock — per-directory mutex for log compaction
// ──────────────────────────────────────────────────────────────────────────────

/// RAII guard for the compaction lock file.
///
/// Uses [`std::fs::OpenOptions::create_new`] for atomic creation (mutual
/// exclusion without `libc`).  Stale locks left by crashed processes are
/// detected via the file's modification time: a lock older than
/// [`STALE_SECS`](CompactLock::STALE_SECS) is assumed abandoned and removed.
struct CompactLock {
    path: std::path::PathBuf,
}

impl CompactLock {
    /// Fallback age threshold (seconds) used on platforms where PID-based
    /// liveness cannot be determined.  5 minutes is conservative enough to
    /// cover realistic compaction durations on large logs.
    const STALE_SECS: u64 = 300;

    /// Read `starttime` (field 22) from `/proc/<pid>/stat`.
    ///
    /// The start time is clock ticks since system boot.  It is unique per
    /// boot: if a PID is recycled the new process will have a different start
    /// time, letting us distinguish a live holder from a reuse.
    #[cfg(target_os = "linux")]
    fn read_proc_start_time(pid: u32) -> Option<u64> {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        // Format: "pid (comm) state ppid pgrp session tty_nr tpgid flags
        //   minflt cminflt majflt cmajflt utime stime cutime cstime
        //   priority nice num_threads itrealvalue starttime ..."
        // `comm` may contain spaces/parens — use rfind to find the last ')'.
        let after_comm = stat.rfind(')')?;
        // After ')': state(0) ppid(1) pgrp(2) session(3) tty_nr(4)
        //   tpgid(5) flags(6) minflt(7) cminflt(8) majflt(9) cmajflt(10)
        //   utime(11) stime(12) cutime(13) cstime(14) priority(15) nice(16)
        //   num_threads(17) itrealvalue(18) starttime(19)
        let fields: Vec<&str> = stat[after_comm + 1..].split_whitespace().collect();
        fields.get(19)?.parse().ok()
    }

    /// On Linux: reads `pid:starttime` from the lock file.  Returns `true`
    /// iff the process is gone **or** its start time differs from the stored
    /// value (PID reuse).  Falls back to age-based check when the lock cannot
    /// be read or parsed.
    #[cfg(target_os = "linux")]
    fn is_stale(lock_path: &Path, meta: &std::fs::Metadata) -> bool {
        if let Some(content) = std::fs::read_to_string(lock_path).ok() {
            let trimmed = content.trim();
            // Lock file format: "pid:starttime" (new) or bare "pid" (legacy).
            let (pid_opt, stored_start_opt) = if let Some((p, s)) = trimmed.split_once(':') {
                (p.parse::<u32>().ok(), s.parse::<u64>().ok())
            } else {
                (trimmed.parse::<u32>().ok(), None)
            };
            if let Some(pid) = pid_opt {
                if !std::path::Path::new(&format!("/proc/{pid}")).exists() {
                    return true; // Process is gone.
                }
                // Guard against PID reuse: a mismatch means the original
                // writer exited and a different process inherited the PID.
                if let (Some(stored), Some(current)) =
                    (stored_start_opt, Self::read_proc_start_time(pid))
                {
                    if stored != current {
                        return true; // PID reused; lock is stale.
                    }
                }
                return false; // Process is live and PID not reused.
            }
        }
        // Cannot read the lock file — fall back to age-based check.
        meta.modified()
            .ok()
            .and_then(|t| t.elapsed().ok())
            .map(|d| d.as_secs() > Self::STALE_SECS)
            .unwrap_or(false)
    }

    /// On non-Linux: falls back to an age-based check with a conservative
    /// threshold so a slow compaction on a large log is not incorrectly evicted.
    #[cfg(not(target_os = "linux"))]
    fn is_stale(_lock_path: &Path, meta: &std::fs::Metadata) -> bool {
        meta.modified()
            .ok()
            .and_then(|t| t.elapsed().ok())
            .map(|d| d.as_secs() > Self::STALE_SECS)
            .unwrap_or(false)
    }

    /// Try to acquire the lock.
    ///
    /// Returns:
    /// - `Ok(Some(guard))` — lock acquired; compaction may proceed.
    /// - `Ok(None)` — lock is held by a live process; caller must skip.
    /// - `Err(_)` — unexpected I/O failure.
    fn try_acquire(lock_path: &Path) -> anyhow::Result<Option<Self>> {
        // Remove stale locks from crashed/killed processes.
        if let Ok(meta) = std::fs::metadata(lock_path) {
            if Self::is_stale(lock_path, &meta) {
                tracing::debug!(
                    path = %lock_path.display(),
                    "event_replay: removing stale compaction lock"
                );
                if let Err(e) = std::fs::remove_file(lock_path) {
                    tracing::warn!(
                        path = %lock_path.display(),
                        "event_replay: failed to remove stale lock: {e}"
                    );
                }
            }
        }

        let pid = std::process::id();
        // On Linux include the process start time so is_stale can detect PID
        // reuse (a recycled PID has a different start time).
        #[cfg(target_os = "linux")]
        let lock_content = {
            let start = Self::read_proc_start_time(pid).unwrap_or(0);
            format!("{pid}:{start}")
        };
        #[cfg(not(target_os = "linux"))]
        let lock_content = pid.to_string();
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(lock_path)
        {
            Ok(mut f) => {
                if let Err(e) = IoWrite::write_all(&mut f, lock_content.as_bytes()) {
                    tracing::warn!(
                        path = %lock_path.display(),
                        "event_replay: failed to write PID to lock file: {e}"
                    );
                }
                Ok(Some(Self {
                    path: lock_path.to_path_buf(),
                }))
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
}

impl Drop for CompactLock {
    fn drop(&mut self) {
        if let Err(e) = std::fs::remove_file(&self.path) {
            tracing::warn!(
                path = %self.path.display(),
                "event_replay: failed to remove compaction lock: {e}"
            );
        }
    }
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

    // Compact the log: remove events for tasks that have reached terminal
    // state — they are now committed to the DB and no longer needed.
    match compact_log(log_path, &states) {
        Ok(removed) if removed > 0 => {
            tracing::info!("event_replay: compacted log, removed {removed} terminal-task line(s)");
        }
        Ok(_) => {}
        Err(e) => {
            // Compaction failure is non-fatal: recovery already succeeded.
            tracing::warn!("event_replay: log compaction failed (non-fatal): {e}");
        }
    }

    Ok(updated)
}

// ──────────────────────────────────────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[path = "event_replay_tests.rs"]
mod tests;
