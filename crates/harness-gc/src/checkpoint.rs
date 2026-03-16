use chrono::{DateTime, Utc};
use harness_core::Event;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use tracing;

/// Persisted state for incremental GC scanning.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GcCheckpoint {
    /// Timestamp of the last successful GC scan.
    pub last_scan_at: DateTime<Utc>,
}

impl GcCheckpoint {
    pub fn new(last_scan_at: DateTime<Utc>) -> Self {
        Self { last_scan_at }
    }

    /// Load checkpoint from `path`. Returns `None` if the file is missing or corrupt.
    pub fn load(path: &Path) -> Option<Self> {
        let data = match std::fs::read_to_string(path) {
            Ok(d) => d,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
            Err(e) => {
                tracing::warn!("gc: failed to read checkpoint at {}: {e}", path.display());
                return None;
            }
        };
        serde_json::from_str(&data)
            .map_err(|e| {
                tracing::warn!("gc: corrupt checkpoint at {}: {e}", path.display());
            })
            .ok()
    }

    /// Save checkpoint to `path`, creating parent directories as needed.
    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let data = serde_json::to_string_pretty(self)?;
        std::fs::write(path, data)?;
        Ok(())
    }
}

/// Return `path` to the default checkpoint file relative to `project_root`.
pub fn default_checkpoint_path(project_root: &Path) -> PathBuf {
    project_root.join(".harness").join("gc-checkpoint.json")
}

/// Filter `events` to only those with `ts > since`. When `since` is `None`,
/// all events are returned (full scan).
pub fn filter_events_since(events: &[Event], since: Option<DateTime<Utc>>) -> &[Event] {
    let Some(cutoff) = since else {
        return events;
    };
    // Events are generally appended in chronological order; find the first one
    // after the cutoff with a linear scan (list is small in practice).
    let pos = events.partition_point(|e| e.ts <= cutoff);
    &events[pos..]
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_core::{Decision, Event, SessionId};

    fn make_event(ts: DateTime<Utc>) -> Event {
        let mut e = Event::new(SessionId::new(), "hook", "tool", Decision::Pass);
        e.ts = ts;
        e
    }

    #[test]
    fn save_and_load_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".harness").join("gc-checkpoint.json");
        let ts = Utc::now();
        let cp = GcCheckpoint::new(ts);

        cp.save(&path).unwrap();
        let loaded = GcCheckpoint::load(&path).expect("checkpoint should load");
        assert_eq!(loaded.last_scan_at, ts);
    }

    #[test]
    fn load_returns_none_for_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nonexistent.json");
        assert!(GcCheckpoint::load(&path).is_none());
    }

    #[test]
    fn load_returns_none_for_corrupt_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.json");
        std::fs::write(&path, b"not valid json").unwrap();
        assert!(GcCheckpoint::load(&path).is_none());
    }

    #[test]
    fn filter_events_since_returns_all_when_no_cutoff() {
        let now = Utc::now();
        let events: Vec<Event> = (0..5)
            .map(|i| make_event(now + chrono::Duration::seconds(i as i64)))
            .collect();
        let filtered = filter_events_since(&events, None);
        assert_eq!(filtered.len(), 5);
    }

    #[test]
    fn filter_events_since_excludes_events_at_or_before_cutoff() {
        let base = Utc::now();
        let events: Vec<Event> = (0..6)
            .map(|i| make_event(base + chrono::Duration::seconds(i as i64)))
            .collect();
        // cutoff is at t+2; only t+3, t+4, t+5 should be included
        let cutoff = base + chrono::Duration::seconds(2);
        let filtered = filter_events_since(&events, Some(cutoff));
        assert_eq!(filtered.len(), 3);
        for e in filtered {
            assert!(e.ts > cutoff);
        }
    }

    #[test]
    fn filter_events_since_returns_empty_when_all_old() {
        let base = Utc::now();
        let events: Vec<Event> = (0..3)
            .map(|i| make_event(base + chrono::Duration::seconds(i as i64)))
            .collect();
        let cutoff = base + chrono::Duration::seconds(10);
        let filtered = filter_events_since(&events, Some(cutoff));
        assert_eq!(filtered.len(), 0);
    }

    #[test]
    fn default_checkpoint_path_is_under_harness_dir() {
        let root = Path::new("/project");
        let path = default_checkpoint_path(root);
        assert_eq!(path, Path::new("/project/.harness/gc-checkpoint.json"));
    }
}
