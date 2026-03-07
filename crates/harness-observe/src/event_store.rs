use harness_core::{
    Decision, Event, EventFilters, EventId, OtelConfig, SessionId, Severity, Violation,
};
use std::path::{Path, PathBuf};

/// Event store backed by JSONL files (SQLite upgrade path available).
pub struct EventStore {
    data_dir: PathBuf,
    otel_pipeline: Option<crate::otel_export::OtelPipeline>,
}

impl EventStore {
    pub fn new(data_dir: &Path) -> anyhow::Result<Self> {
        std::fs::create_dir_all(data_dir)?;
        Ok(Self {
            data_dir: data_dir.to_path_buf(),
            otel_pipeline: None,
        })
    }

    pub fn with_policies(
        data_dir: &Path,
        session_renewal_secs: u64,
        log_retention_days: u32,
    ) -> anyhow::Result<Self> {
        Self::with_policies_and_otel(
            data_dir,
            session_renewal_secs,
            log_retention_days,
            &OtelConfig::default(),
        )
    }

    pub fn with_policies_and_otel(
        data_dir: &Path,
        _session_renewal_secs: u64,
        _log_retention_days: u32,
        otel_config: &OtelConfig,
    ) -> anyhow::Result<Self> {
        let mut store = Self::new(data_dir)?;
        store.otel_pipeline = match crate::otel_export::OtelPipeline::from_config(otel_config) {
            Ok(pipeline) => pipeline,
            Err(err) => {
                tracing::warn!(
                    "OpenTelemetry initialization failed; continuing without export: {err}"
                );
                None
            }
        };
        Ok(store)
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
        if let Some(pipeline) = &self.otel_pipeline {
            pipeline.record_event(event);
        }
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

    /// Persist a full rule scan into the event store.
    ///
    /// - Always logs a single `rule_scan` event (even when `violations` is empty)
    /// - Logs one `rule_check` event per violation under the same `session_id`
    pub fn persist_rule_scan(&self, project_root: &Path, violations: &[Violation]) -> SessionId {
        let session_id = SessionId::new();
        let decision = if violations.is_empty() {
            Decision::Pass
        } else {
            Decision::Warn
        };
        let mut scan_event = Event::new(session_id.clone(), "rule_scan", "RuleEngine", decision);
        scan_event.reason = Some(format!("violations={}", violations.len()));
        scan_event.detail = Some(project_root.display().to_string());
        if let Err(e) = self.log(&scan_event) {
            tracing::warn!("failed to log rule_scan event: {e}");
        }

        self.log_violations_with_session(&session_id, violations);
        session_id
    }

    fn log_violations_with_session(&self, session_id: &SessionId, violations: &[Violation]) {
        if violations.is_empty() {
            return;
        }

        for violation in violations {
            let decision = match violation.severity {
                Severity::Critical | Severity::High => Decision::Block,
                Severity::Medium => Decision::Warn,
                Severity::Low => Decision::Pass,
            };
            let mut event = Event::new(
                session_id.clone(),
                "rule_check",
                violation.rule_id.as_str(),
                decision,
            );
            event.reason = Some(violation.message.clone());
            event.detail = Some(if let Some(line) = violation.line {
                format!("{}:{}", violation.file.display(), line)
            } else {
                violation.file.display().to_string()
            });
            if let Err(e) = self.log(&event) {
                tracing::warn!("failed to log rule violation event: {e}");
            }
        }
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
        if let Some(ref tool) = filters.tool {
            if event.tool != *tool {
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
    use harness_core::{Decision, Event, EventFilters, RuleId, SessionId};
    use std::path::Path;

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
    fn query_filters_by_tool() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = EventStore::new(dir.path())?;
        let sid = SessionId::new();
        store.log(&Event::new(sid.clone(), "hook", "tool_a", Decision::Pass))?;
        store.log(&Event::new(sid, "hook", "tool_b", Decision::Pass))?;
        let results = store.query(&EventFilters {
            tool: Some("tool_a".to_string()),
            ..Default::default()
        })?;
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].tool, "tool_a");
        Ok(())
    }

    #[test]
    fn query_respects_limit() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = EventStore::new(dir.path())?;
        for _ in 0..5 {
            store.log(&make_event("hook", Decision::Pass))?;
        }
        let results = store.query(&EventFilters {
            limit: Some(3),
            ..Default::default()
        })?;
        assert_eq!(results.len(), 3);
        Ok(())
    }

    #[test]
    fn persist_rule_scan_logs_one_event_per_violation_under_scan_session() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = EventStore::new(dir.path())?;
        let violations = vec![
            Violation {
                rule_id: RuleId::from_str("SEC-01"),
                file: std::path::PathBuf::from("src/main.rs"),
                line: Some(42),
                message: "security issue".to_string(),
                severity: Severity::Critical,
            },
            Violation {
                rule_id: RuleId::from_str("U-05"),
                file: std::path::PathBuf::from("src/lib.rs"),
                line: None,
                message: "style issue".to_string(),
                severity: Severity::Low,
            },
        ];
        let session_id = store.persist_rule_scan(Path::new("/tmp/project"), &violations);
        let events = store.query(&EventFilters::default())?;
        assert_eq!(events.len(), 3);
        assert_eq!(events.iter().filter(|e| e.hook == "rule_scan").count(), 1);
        let check_events: Vec<_> = events.iter().filter(|e| e.hook == "rule_check").collect();
        assert_eq!(check_events.len(), 2);
        assert!(check_events
            .iter()
            .all(|event| event.session_id == session_id));
        Ok(())
    }

    #[test]
    fn persist_rule_scan_logs_summary_even_when_empty() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = EventStore::new(dir.path())?;
        store.persist_rule_scan(Path::new("/tmp/project"), &[]);
        let scan_events = store.query(&EventFilters {
            hook: Some("rule_scan".to_string()),
            ..Default::default()
        })?;
        assert_eq!(scan_events.len(), 1);
        assert_eq!(scan_events[0].decision, Decision::Pass);

        let violation_events = store.query(&EventFilters {
            hook: Some("rule_check".to_string()),
            ..Default::default()
        })?;
        assert!(violation_events.is_empty());
        Ok(())
    }

    #[test]
    fn persist_rule_scan_maps_severity_to_decision() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = EventStore::new(dir.path())?;
        let violations = vec![
            Violation {
                rule_id: RuleId::from_str("R-CRIT"),
                file: std::path::PathBuf::from("a.rs"),
                line: None,
                message: "critical".to_string(),
                severity: Severity::Critical,
            },
            Violation {
                rule_id: RuleId::from_str("R-HIGH"),
                file: std::path::PathBuf::from("b.rs"),
                line: None,
                message: "high".to_string(),
                severity: Severity::High,
            },
            Violation {
                rule_id: RuleId::from_str("R-MED"),
                file: std::path::PathBuf::from("c.rs"),
                line: None,
                message: "medium".to_string(),
                severity: Severity::Medium,
            },
            Violation {
                rule_id: RuleId::from_str("R-LOW"),
                file: std::path::PathBuf::from("d.rs"),
                line: None,
                message: "low".to_string(),
                severity: Severity::Low,
            },
        ];
        store.persist_rule_scan(Path::new("/tmp/project"), &violations);
        let events = store.query(&EventFilters {
            hook: Some("rule_check".to_string()),
            ..Default::default()
        })?;
        assert_eq!(events.len(), 4);
        let by_tool: std::collections::HashMap<_, _> = events
            .iter()
            .map(|e| (e.tool.as_str(), e.decision.clone()))
            .collect();
        assert_eq!(by_tool["R-CRIT"], Decision::Block);
        assert_eq!(by_tool["R-HIGH"], Decision::Block);
        assert_eq!(by_tool["R-MED"], Decision::Warn);
        assert_eq!(by_tool["R-LOW"], Decision::Pass);
        Ok(())
    }

    #[test]
    fn persist_rule_scan_stores_project_path_on_anchor() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = EventStore::new(dir.path())?;
        let project_root = Path::new("/tmp/my-project");
        store.persist_rule_scan(project_root, &[]);
        let events = store.query(&EventFilters {
            hook: Some("rule_scan".to_string()),
            ..Default::default()
        })?;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].detail.as_deref(), Some("/tmp/my-project"));
        Ok(())
    }
}
