use harness_core::{Event, EventFilters, EventId};
use std::path::{Path, PathBuf};

/// Event store backed by JSONL files (SQLite upgrade path available).
pub struct EventStore {
    data_dir: PathBuf,
}

impl EventStore {
    pub fn new(data_dir: &Path) -> anyhow::Result<Self> {
        std::fs::create_dir_all(data_dir)?;
        Ok(Self {
            data_dir: data_dir.to_path_buf(),
        })
    }

    fn events_file(&self) -> PathBuf {
        self.data_dir.join("events.jsonl")
    }

    pub fn log(&self, event: &Event) -> anyhow::Result<EventId> {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.events_file())?;
        let line = serde_json::to_string(event)?;
        writeln!(file, "{line}")?;
        Ok(event.id.clone())
    }

    pub fn query(&self, filters: &EventFilters) -> anyhow::Result<Vec<Event>> {
        let path = self.events_file();
        if !path.exists() {
            return Ok(Vec::new());
        }

        let content = std::fs::read_to_string(&path)?;
        let mut events: Vec<Event> = Vec::new();

        for line in content.lines() {
            if line.trim().is_empty() {
                continue;
            }
            if let Ok(event) = serde_json::from_str::<Event>(line) {
                if Self::matches_filters(&event, filters) {
                    events.push(event);
                }
            }
        }

        if let Some(limit) = filters.limit {
            events.truncate(limit);
        }

        Ok(events)
    }

    pub fn query_recent(&self, duration: std::time::Duration) -> anyhow::Result<Vec<Event>> {
        let since = chrono::Utc::now() - chrono::Duration::from_std(duration)?;
        self.query(&EventFilters {
            since: Some(since),
            ..Default::default()
        })
    }

    fn matches_filters(event: &Event, filters: &EventFilters) -> bool {
        if let Some(ref sid) = filters.session_id {
            if event.session_id != *sid {
                return false;
            }
        }
        if let Some(ref hook) = filters.hook {
            if event.hook != *hook {
                return false;
            }
        }
        if let Some(ref decision) = filters.decision {
            if event.decision != *decision {
                return false;
            }
        }
        if let Some(ref since) = filters.since {
            if event.ts < *since {
                return false;
            }
        }
        if let Some(ref until) = filters.until {
            if event.ts > *until {
                return false;
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_core::{Decision, Event, EventFilters, SessionId};

    fn make_event(hook: &str, decision: Decision) -> Event {
        Event::new(SessionId::new(), hook, "Edit", decision)
    }

    #[test]
    fn query_empty_store_returns_empty() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = EventStore::new(dir.path())?;
        let results = store.query(&EventFilters::default())?;
        assert!(results.is_empty());
        Ok(())
    }

    #[test]
    fn log_and_query_roundtrip() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = EventStore::new(dir.path())?;
        let event = make_event("pre_tool_use", Decision::Pass);
        store.log(&event)?;
        let results = store.query(&EventFilters::default())?;
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, event.id);
        Ok(())
    }

    #[test]
    fn query_filters_by_hook() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = EventStore::new(dir.path())?;
        store.log(&make_event("pre_tool_use", Decision::Pass))?;
        store.log(&make_event("post_tool_use", Decision::Pass))?;
        let results = store.query(&EventFilters {
            hook: Some("pre_tool_use".to_string()),
            ..Default::default()
        })?;
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].hook, "pre_tool_use");
        Ok(())
    }

    #[test]
    fn query_filters_by_decision() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = EventStore::new(dir.path())?;
        store.log(&make_event("h1", Decision::Pass))?;
        store.log(&make_event("h2", Decision::Block))?;
        let results = store.query(&EventFilters {
            decision: Some(Decision::Block),
            ..Default::default()
        })?;
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].decision, Decision::Block);
        Ok(())
    }

    #[test]
    fn query_respects_limit() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = EventStore::new(dir.path())?;
        for _ in 0..5 {
            store.log(&make_event("hook", Decision::Pass))?;
        }
        let results = store.query(&EventFilters { limit: Some(3), ..Default::default() })?;
        assert_eq!(results.len(), 3);
        Ok(())
    }
}
