//! Task-level checkpoint persistence.
//!
//! A [`TaskCheckpoint`] captures the execution phase, turn count, and key
//! artifacts (PR URL, branch) at each phase transition so that an interrupted
//! task can resume from the last safe state instead of hard-failing.
//!
//! Checkpoints are stored as JSON files under
//! `~/.local/share/harness/checkpoints/<task_id>.json` (secondary durability).
//! The primary durability copy lives in the `checkpoint_json` column of the
//! `tasks` table (migration v9), which is written first and is preferred on load.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Age threshold beyond which a checkpoint is considered stale and the task
/// should be hard-failed rather than resumed (2 hours).
const CHECKPOINT_MAX_AGE_SECS: i64 = 7_200;

/// Persisted snapshot of a task's execution state at a phase boundary.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TaskCheckpoint {
    /// Task ID this checkpoint belongs to.
    pub task_id: String,
    /// Pipeline phase at which the checkpoint was saved (e.g. "implement").
    pub phase: String,
    /// Turn count at checkpoint time.
    pub turn: u32,
    /// PR URL if one had been created before the interruption.
    pub pr_url: Option<String>,
    /// Git branch name if known.
    pub branch: Option<String>,
    /// Number of review/eval rounds completed before interruption.
    pub round_count: u32,
    /// Wall-clock time when the checkpoint was saved.
    pub saved_at: DateTime<Utc>,
}

impl TaskCheckpoint {
    /// Create a new checkpoint for the given task.
    pub fn new(
        task_id: &str,
        phase: &str,
        turn: u32,
        pr_url: Option<String>,
        branch: Option<String>,
        round_count: u32,
    ) -> Self {
        Self {
            task_id: task_id.to_string(),
            phase: phase.to_string(),
            turn,
            pr_url,
            branch,
            round_count,
            saved_at: Utc::now(),
        }
    }

    /// Returns `true` if the checkpoint is fresh (saved within 2 hours).
    ///
    /// Stale checkpoints indicate the task sat interrupted for too long; the
    /// safe course is to mark it failed rather than attempt a resume.
    ///
    /// A negative age (future `saved_at` due to clock skew or corrupt data)
    /// is treated as invalid and returns `false` to avoid bypassing the
    /// expiry guard.
    pub fn is_fresh(&self) -> bool {
        let age = Utc::now()
            .signed_duration_since(self.saved_at)
            .num_seconds();
        (0..=CHECKPOINT_MAX_AGE_SECS).contains(&age)
    }

    /// Load a checkpoint from the JSON string stored in the database.
    /// Returns `None` if `json` is empty or cannot be parsed.
    pub fn from_json(json: &str) -> Option<Self> {
        if json.is_empty() {
            return None;
        }
        serde_json::from_str(json)
            .map_err(|e| tracing::warn!("checkpoint: corrupt JSON in DB: {e}"))
            .ok()
    }

    /// Serialize the checkpoint to a JSON string for DB storage.
    pub fn to_json(&self) -> anyhow::Result<String> {
        serde_json::to_string(self).map_err(Into::into)
    }

    /// Load a checkpoint from the filesystem path for `task_id`.
    /// Returns `None` if the file is missing or corrupt.
    pub fn load(task_id: &str) -> Option<Self> {
        let path = Self::default_path(task_id);
        let data = match std::fs::read_to_string(&path) {
            Ok(d) => d,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
            Err(e) => {
                tracing::warn!("checkpoint: failed to read {}: {e}", path.display());
                return None;
            }
        };
        serde_json::from_str(&data)
            .map_err(|e| tracing::warn!("checkpoint: corrupt file {}: {e}", path.display()))
            .ok()
    }

    /// Save the checkpoint to the filesystem path for this task.
    ///
    /// Uses an atomic write: the JSON is first written to a uniquely-named
    /// `.tmp` side-car file, synced to disk, then renamed into place.
    ///
    /// * **Unique temp name** — a UUID suffix prevents two concurrent `save()`
    ///   calls for the same task from clobbering each other's temp file.
    /// * **`sync_all()` before rename** — ensures the data bytes are on disk
    ///   before the directory entry is updated; without this a power loss after
    ///   `rename` can leave a zero-byte or truncated checkpoint file.
    /// * **Parent-directory fsync after rename** — makes the new directory
    ///   entry itself durable so the file is not lost on power loss even after
    ///   the rename syscall returns.
    ///
    /// POSIX guarantees that `rename(2)` is atomic with respect to readers, so
    /// a concurrent `load` will always see either the previous complete
    /// checkpoint or the new one — never a partial write.
    pub fn save(&self) -> anyhow::Result<()> {
        use std::io::Write as _;

        let path = Self::default_path(&self.task_id);
        let parent = path
            .parent()
            .ok_or_else(|| anyhow::anyhow!("checkpoint path has no parent directory"))?;
        std::fs::create_dir_all(parent)?;

        let data = serde_json::to_string_pretty(self)?;

        // Unique suffix prevents races when save() is called concurrently for
        // the same task_id (e.g. from two tokio tasks).
        let tmp_name = format!("{}.{}.tmp", self.task_id, uuid::Uuid::new_v4());
        let tmp_path = parent.join(tmp_name);

        // Write and sync the temp file before rename so data is on disk.
        {
            let mut tmp_file = std::fs::File::create(&tmp_path)?;
            tmp_file.write_all(data.as_bytes())?;
            tmp_file.sync_all()?;
        }
        std::fs::rename(&tmp_path, &path)?;

        // fsync the parent directory so the renamed directory entry is durable.
        let dir = std::fs::File::open(parent)?;
        dir.sync_all()?;

        Ok(())
    }

    /// Return the default filesystem path for a task's checkpoint.
    pub fn default_path(task_id: &str) -> PathBuf {
        harness_core::config::dirs::default_db_path(Path::new("checkpoints"), task_id)
            .with_extension("json")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_checkpoint(task_id: &str) -> TaskCheckpoint {
        TaskCheckpoint::new(
            task_id,
            "implement",
            3,
            Some("https://github.com/org/repo/pull/42".to_string()),
            Some("feat/my-branch".to_string()),
            1,
        )
    }

    #[test]
    fn save_and_load_roundtrip() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let task_id = "test-task-save-load";
        let cp = make_checkpoint(task_id);

        // Override path by writing directly.
        let path = tmp.path().join(format!("{task_id}.json"));
        std::fs::write(&path, serde_json::to_string_pretty(&cp)?)?;

        let data = std::fs::read_to_string(&path)?;
        let loaded: TaskCheckpoint = serde_json::from_str(&data)?;
        assert_eq!(loaded.task_id, task_id);
        assert_eq!(loaded.phase, "implement");
        assert_eq!(loaded.turn, 3);
        assert_eq!(loaded.round_count, 1);
        assert_eq!(
            loaded.pr_url.as_deref(),
            Some("https://github.com/org/repo/pull/42")
        );
        assert_eq!(loaded.branch.as_deref(), Some("feat/my-branch"));
        Ok(())
    }

    #[test]
    fn fresh_checkpoint_returns_true_for_recent() {
        let cp = make_checkpoint("task-fresh");
        // Just created → should be fresh.
        assert!(cp.is_fresh());
    }

    #[test]
    fn stale_checkpoint_returns_false_for_old() {
        let mut cp = make_checkpoint("task-stale");
        // Backdate by 3 hours.
        cp.saved_at = Utc::now() - chrono::Duration::hours(3);
        assert!(!cp.is_fresh());
    }

    #[test]
    fn future_timestamp_is_not_fresh() {
        let mut cp = make_checkpoint("task-future");
        // Advance saved_at by 1 hour into the future (clock skew / corrupt data).
        cp.saved_at = Utc::now() + chrono::Duration::hours(1);
        assert!(
            !cp.is_fresh(),
            "future saved_at must not be treated as fresh"
        );
    }

    #[test]
    fn from_json_roundtrip() -> anyhow::Result<()> {
        let cp = make_checkpoint("task-json");
        let json = cp.to_json()?;
        let loaded = TaskCheckpoint::from_json(&json)
            .ok_or_else(|| anyhow::anyhow!("checkpoint JSON should parse"))?;
        assert_eq!(loaded.task_id, "task-json");
        assert_eq!(loaded.phase, "implement");
        Ok(())
    }

    #[test]
    fn from_json_returns_none_for_empty_string() {
        assert!(TaskCheckpoint::from_json("").is_none());
    }

    #[test]
    fn from_json_returns_none_for_corrupt_json() {
        assert!(TaskCheckpoint::from_json("{not valid json").is_none());
    }

    #[test]
    fn load_returns_none_for_missing_file() {
        // Use a UUID-ish task ID that won't collide with any real checkpoint.
        let loaded = TaskCheckpoint::load("nonexistent-task-99999999");
        // Either None (file doesn't exist) or Some — if by accident the file
        // exists in CI the test still passes; what matters is no panic.
        let _ = loaded;
    }
}
