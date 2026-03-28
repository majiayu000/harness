use chrono::{DateTime, Utc};
use harness_core::config::misc::OtelConfig;
use harness_core::db::{open_pool, Migration, Migrator};
use harness_core::types::{
    AutoFixReport, Decision, Event, EventFilters, EventId, ExternalSignal, ExternalSignalId, Grade,
    SessionId, Severity, Violation,
};
use sqlx::sqlite::SqlitePool;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Versioned migrations for the events table.
static EVENT_MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        description: "create events table with indexes",
        sql: "CREATE TABLE IF NOT EXISTS events (
        id          TEXT PRIMARY KEY,
        ts          TEXT NOT NULL,
        session_id  TEXT NOT NULL,
        hook        TEXT NOT NULL,
        tool        TEXT NOT NULL,
        decision    TEXT NOT NULL,
        reason      TEXT,
        detail      TEXT,
        duration_ms INTEGER
    );
    CREATE INDEX IF NOT EXISTS idx_events_session_id ON events (session_id);
    CREATE INDEX IF NOT EXISTS idx_events_hook ON events (hook);
    CREATE INDEX IF NOT EXISTS idx_events_decision ON events (decision);
    CREATE INDEX IF NOT EXISTS idx_events_ts ON events (ts)",
    },
    Migration {
        version: 2,
        description: "add content column to events table",
        sql: "ALTER TABLE events ADD COLUMN content TEXT",
    },
];

/// Event store backed by SQLite (same database as other harness stores).
///
/// Backward compatibility: on first startup the store imports any existing
/// `events.jsonl` file found in the data directory, then leaves it in place as
/// an archive.
pub struct EventStore {
    pool: SqlitePool,
    data_dir: PathBuf,
    otel_pipeline: Mutex<Option<crate::otel_export::OtelPipeline>>,
    /// Number of seconds of inactivity before a new session is considered started.
    /// Exposed via `session_renewal_secs()` for callers that need to group events.
    session_renewal_secs: u64,
}

impl EventStore {
    pub async fn new(data_dir: &Path) -> anyhow::Result<Self> {
        std::fs::create_dir_all(data_dir)?;
        // Do NOT canonicalize here: on Windows, std::fs::canonicalize produces
        // a `\\?\`-prefixed UNC path whose `?` would truncate the SQLite URL
        // query string (e.g. `sqlite:\\?\C:\...\events.db?mode=rwc` becomes
        // `sqlite:\\?\C:\...\events.db`), causing the DB open to fail.
        // The primary path-traversal boundary is at task_routes.rs.
        let data_dir = data_dir.to_path_buf();
        let db_path = data_dir.join("events.db");
        let pool = open_pool(&db_path).await?;
        Migrator::new(&pool, EVENT_MIGRATIONS).run().await?;

        let store = Self {
            pool,
            data_dir,
            otel_pipeline: Mutex::new(None),
            session_renewal_secs: 1800,
        };

        store.migrate_from_jsonl().await;
        Ok(store)
    }

    /// Close the SQLite connection pool.
    ///
    /// This is useful in tests to ensure clean shutdown before tempdir cleanup.
    pub async fn close(self) {
        self.pool.close().await;
    }

    /// Return the configured session-renewal window in seconds.
    pub fn session_renewal_secs(&self) -> u64 {
        self.session_renewal_secs
    }

    /// Delete all events whose timestamp is older than `days` days.
    ///
    /// Returns the number of rows deleted.  A `days` value of 0 is a no-op.
    pub async fn purge_old_events(&self, days: u32) -> anyhow::Result<u64> {
        if days == 0 {
            return Ok(0);
        }
        let cutoff = chrono::Utc::now() - chrono::Duration::days(i64::from(days));
        let cutoff_str = cutoff.to_rfc3339();
        let result = sqlx::query("DELETE FROM events WHERE ts < ?")
            .bind(&cutoff_str)
            .execute(&self.pool)
            .await?;
        let deleted = result.rows_affected();
        if deleted > 0 {
            tracing::info!(deleted, days, "event store: purged old events");
        }
        Ok(deleted)
    }

    pub async fn with_policies_and_otel(
        data_dir: &Path,
        session_renewal_secs: u64,
        log_retention_days: u32,
        otel_config: &OtelConfig,
    ) -> anyhow::Result<Self> {
        let mut store = Self::new(data_dir).await?;
        store.session_renewal_secs = session_renewal_secs;
        tracing::debug!(
            session_renewal_secs,
            log_retention_days,
            "event store: applying retention policies"
        );
        if let Err(e) = store.purge_old_events(log_retention_days).await {
            tracing::warn!("event store: failed to purge old events: {e}");
        }
        let pipeline = match crate::otel_export::OtelPipeline::from_config(otel_config).await {
            Ok(pipeline) => pipeline,
            Err(err) => {
                tracing::warn!(
                    "OpenTelemetry initialization failed; continuing without export: {err}"
                );
                None
            }
        };
        match store.otel_pipeline.lock() {
            Ok(mut slot) => *slot = pipeline,
            Err(poisoned) => {
                tracing::error!("OpenTelemetry pipeline mutex poisoned during init; recovering");
                *poisoned.into_inner() = pipeline;
            }
        }
        Ok(store)
    }

    /// Import events from an existing `events.jsonl` file (backward compat).
    ///
    /// Silently skips lines that fail to parse or insert (duplicate id = IGNORE).
    async fn migrate_from_jsonl(&self) {
        let path = self.data_dir.join("events.jsonl");
        if !path.exists() {
            return;
        }
        let contents = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("event store: could not read events.jsonl for migration: {e}");
                return;
            }
        };
        let mut imported = 0usize;
        for line in contents.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            if let Ok(event) = serde_json::from_str::<Event>(line) {
                if let Err(e) = self.insert_event(&event).await {
                    tracing::warn!("event store: failed to insert migrated event: {e}");
                } else {
                    imported += 1;
                }
            }
        }
        if imported > 0 {
            tracing::info!(
                imported,
                "event store: migrated events from events.jsonl to SQLite"
            );
        }
    }

    async fn insert_event(&self, event: &Event) -> anyhow::Result<()> {
        let decision = serde_json::to_string(&event.decision)?;
        let decision = decision.trim_matches('"');
        let ts = event.ts.to_rfc3339();
        sqlx::query(
            "INSERT OR IGNORE INTO events
                (id, ts, session_id, hook, tool, decision, reason, detail, duration_ms, content)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(event.id.as_str())
        .bind(&ts)
        .bind(event.session_id.as_str())
        .bind(&event.hook)
        .bind(&event.tool)
        .bind(decision)
        .bind(&event.reason)
        .bind(&event.detail)
        .bind(event.duration_ms.map(|v| v as i64))
        .bind(&event.content)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn log(&self, event: &Event) -> anyhow::Result<EventId> {
        self.insert_event(event).await?;
        match self.otel_pipeline.lock() {
            Ok(slot) => {
                if let Some(pipeline) = slot.as_ref() {
                    pipeline.record_event(event);
                }
            }
            Err(poisoned) => {
                tracing::error!("OpenTelemetry pipeline mutex poisoned during log; recovering");
                let slot = poisoned.into_inner();
                if let Some(pipeline) = slot.as_ref() {
                    pipeline.record_event(event);
                }
            }
        }
        Ok(event.id.clone())
    }

    pub async fn query(&self, filters: &EventFilters) -> anyhow::Result<Vec<Event>> {
        let mut conditions: Vec<&str> = Vec::new();
        // Only load the `content` column when explicitly requested; it can be large and
        // is not needed by hot paths (dashboard, health, gc, quality_trigger).
        let content_col = if filters.include_content {
            "content"
        } else {
            "NULL as content"
        };
        let mut sql = format!(
            "SELECT id, ts, session_id, hook, tool, decision, reason, detail, duration_ms, {content_col}
             FROM events WHERE 1=1",
        );

        if filters.session_id.is_some() {
            conditions.push(" AND session_id = ?");
        }
        if filters.hook.is_some() {
            conditions.push(" AND hook = ?");
        }
        if filters.tool.is_some() {
            conditions.push(" AND tool = ?");
        }
        if filters.decision.is_some() {
            conditions.push(" AND decision = ?");
        }
        if filters.since.is_some() {
            conditions.push(" AND ts >= ?");
        }
        if filters.until.is_some() {
            conditions.push(" AND ts <= ?");
        }

        for cond in &conditions {
            sql.push_str(cond);
        }
        sql.push_str(" ORDER BY ts ASC");

        if let Some(limit) = filters.limit {
            sql.push_str(&format!(" LIMIT {limit}"));
        }

        let mut q = sqlx::query(&sql);

        if let Some(ref sid) = filters.session_id {
            q = q.bind(sid.as_str());
        }
        if let Some(ref hook) = filters.hook {
            q = q.bind(hook.as_str());
        }
        if let Some(ref tool) = filters.tool {
            q = q.bind(tool.as_str());
        }
        if let Some(ref decision) = filters.decision {
            let d = serde_json::to_string(decision)?;
            let d = d.trim_matches('"').to_string();
            q = q.bind(d);
        }
        if let Some(ref since) = filters.since {
            q = q.bind(since.to_rfc3339());
        }
        if let Some(ref until) = filters.until {
            q = q.bind(until.to_rfc3339());
        }

        let rows = q.fetch_all(&self.pool).await?;
        let mut events = Vec::with_capacity(rows.len());
        for row in rows {
            let event = Self::row_to_event(&row)?;
            events.push(event);
        }
        Ok(events)
    }

    fn row_to_event(row: &sqlx::sqlite::SqliteRow) -> anyhow::Result<Event> {
        use sqlx::Row;
        let id: String = row.try_get("id")?;
        let ts_str: String = row.try_get("ts")?;
        let session_id: String = row.try_get("session_id")?;
        let hook: String = row.try_get("hook")?;
        let tool: String = row.try_get("tool")?;
        let decision_str: String = row.try_get("decision")?;
        let reason: Option<String> = row.try_get("reason")?;
        let detail: Option<String> = row.try_get("detail")?;
        let content: Option<String> = row.try_get("content")?;
        let duration_ms: Option<i64> = row.try_get("duration_ms")?;

        let ts = chrono::DateTime::parse_from_rfc3339(&ts_str)
            .map_err(|e| anyhow::anyhow!("invalid ts '{ts_str}': {e}"))?
            .with_timezone(&chrono::Utc);

        let decision: Decision = serde_json::from_str(&format!("\"{decision_str}\""))
            .map_err(|e| anyhow::anyhow!("invalid decision '{decision_str}': {e}"))?;

        Ok(Event {
            id: EventId::from_str(&id),
            ts,
            session_id: SessionId::from_str(&session_id),
            hook,
            tool,
            decision,
            reason,
            detail,
            content,
            duration_ms: duration_ms.map(|v| v as u64),
        })
    }

    pub async fn shutdown(&self) {
        let pipeline = match self.otel_pipeline.lock() {
            Ok(mut slot) => slot.take(),
            Err(poisoned) => {
                tracing::error!(
                    "OpenTelemetry pipeline mutex poisoned during shutdown; recovering"
                );
                poisoned.into_inner().take()
            }
        };
        if let Some(pipeline) = pipeline {
            pipeline.shutdown().await;
        }
    }

    /// Persist a full rule scan into the event store.
    ///
    /// - Always logs a single `rule_scan` event (even when `violations` is empty)
    /// - Logs one `rule_check` event per violation under the same `session_id`
    pub async fn persist_rule_scan(
        &self,
        project_root: &Path,
        violations: &[Violation],
    ) -> SessionId {
        let session_id = SessionId::new();
        let decision = if violations.is_empty() {
            Decision::Pass
        } else {
            Decision::Warn
        };
        let mut scan_event = Event::new(session_id.clone(), "rule_scan", "RuleEngine", decision);
        scan_event.reason = Some(format!("violations={}", violations.len()));
        scan_event.detail = Some(project_root.display().to_string());
        if let Err(e) = self.log(&scan_event).await {
            tracing::warn!("failed to log rule_scan event: {e}");
        }

        self.log_violations_with_session(&session_id, violations)
            .await;
        session_id
    }

    async fn log_violations_with_session(&self, session_id: &SessionId, violations: &[Violation]) {
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
            if let Err(e) = self.log(&event).await {
                tracing::warn!("failed to log rule violation event: {e}");
            }
        }
    }

    /// Log a quality grade event for trend tracking.
    ///
    /// Uses the "quality_grade" hook with decision mapped from grade:
    /// A/B → Pass, C → Warn, D → Block.
    pub async fn log_quality_grade(&self, grade: Grade, score: f64) {
        let decision = match grade {
            Grade::A | Grade::B => Decision::Pass,
            Grade::C => Decision::Warn,
            Grade::D => Decision::Block,
        };
        let mut event = Event::new(SessionId::new(), "quality_grade", "QualityGrader", decision);
        event.detail = Some(format!("grade={grade:?} score={score:.1}"));
        if let Err(e) = self.log(&event).await {
            tracing::warn!("failed to log quality_grade event: {e}");
        }
    }

    /// Log all auto-fix attempts and the overall outcome from a `scan_and_fix` run.
    ///
    /// Emits one `auto_fix` summary event followed by one `auto_fix_attempt` event per
    /// violation that had a fix_pattern. Uses `Decision::Pass` when all violations were
    /// resolved, `Decision::Warn` when some residual violations remain.
    pub async fn log_auto_fix_report(
        &self,
        session_id: &SessionId,
        report: &AutoFixReport,
        project_root: &std::path::Path,
    ) {
        let decision = if report.residual_violations.is_empty() {
            Decision::Pass
        } else {
            Decision::Warn
        };
        let mut summary = Event::new(session_id.clone(), "auto_fix", "RuleEngine", decision);
        summary.reason = Some(format!(
            "applied={} residual={}",
            report.fixed_count,
            report.residual_violations.len()
        ));
        summary.detail = Some(project_root.display().to_string());
        if let Err(e) = self.log(&summary).await {
            tracing::warn!("failed to log auto_fix event: {e}");
        }

        for attempt in &report.attempts {
            let attempt_decision = if attempt.resolved {
                Decision::Pass
            } else if attempt.applied {
                Decision::Warn
            } else {
                Decision::Block
            };
            let mut evt = Event::new(
                session_id.clone(),
                "auto_fix_attempt",
                attempt.rule_id.as_str(),
                attempt_decision,
            );
            evt.reason = Some(format!(
                "applied={} resolved={}",
                attempt.applied, attempt.resolved
            ));
            evt.detail = Some(if let Some(line) = attempt.line {
                format!("{}:{line}", attempt.file.display())
            } else {
                attempt.file.display().to_string()
            });
            if let Err(e) = self.log(&evt).await {
                tracing::warn!("failed to log auto_fix_attempt event: {e}");
            }
        }
    }

    pub async fn query_recent(&self, duration: std::time::Duration) -> anyhow::Result<Vec<Event>> {
        let since = chrono::Utc::now() - chrono::Duration::from_std(duration)?;
        self.query(&EventFilters {
            since: Some(since),
            ..Default::default()
        })
        .await
    }

    fn signals_file(&self) -> std::path::PathBuf {
        self.data_dir.join("signals.jsonl")
    }

    /// Persist an external signal to the signals log.
    pub fn log_external_signal(&self, signal: &ExternalSignal) -> anyhow::Result<ExternalSignalId> {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.signals_file())?;
        let line = serde_json::to_string(signal)?;
        writeln!(file, "{line}")?;
        Ok(signal.id.clone())
    }

    /// Query persisted external signals, optionally filtered by receive time.
    pub fn query_external_signals(
        &self,
        since: Option<DateTime<Utc>>,
    ) -> anyhow::Result<Vec<ExternalSignal>> {
        let path = self.signals_file();
        if !path.exists() {
            return Ok(Vec::new());
        }
        let file = std::fs::File::open(&path)?;
        let reader = std::io::BufReader::new(file);
        let mut signals = Vec::new();
        use std::io::BufRead;
        for line in reader.lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            if let Ok(sig) = serde_json::from_str::<ExternalSignal>(&line) {
                if let Some(since) = since {
                    if sig.received_at < since {
                        continue;
                    }
                }
                signals.push(sig);
            }
        }
        Ok(signals)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_core::{
        AutoFixAttempt, AutoFixReport, Decision, Event, EventFilters, ExternalSignal, OtelExporter,
        RuleId, SessionId, Severity,
    };
    use std::path::Path;

    fn make_event(hook: &str, decision: Decision) -> Event {
        Event::new(SessionId::new(), hook, "Edit", decision)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn query_empty_store_returns_empty() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = EventStore::new(dir.path()).await?;
        let results = store.query(&EventFilters::default()).await?;
        assert!(results.is_empty());
        store.close().await;
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn log_and_query_roundtrip() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = EventStore::new(dir.path()).await?;
        let event = make_event("pre_tool_use", Decision::Pass);
        store.log(&event).await?;
        let results = store.query(&EventFilters::default()).await?;
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, event.id);
        store.close().await;
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn query_filters_by_hook() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = EventStore::new(dir.path()).await?;
        store
            .log(&make_event("pre_tool_use", Decision::Pass))
            .await?;
        store
            .log(&make_event("post_tool_use", Decision::Pass))
            .await?;
        let results = store
            .query(&EventFilters {
                hook: Some("pre_tool_use".to_string()),
                ..Default::default()
            })
            .await?;
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].hook, "pre_tool_use");
        store.close().await;
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn query_filters_by_decision() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = EventStore::new(dir.path()).await?;
        store.log(&make_event("h1", Decision::Pass)).await?;
        store.log(&make_event("h2", Decision::Block)).await?;
        let results = store
            .query(&EventFilters {
                decision: Some(Decision::Block),
                ..Default::default()
            })
            .await?;
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].decision, Decision::Block);
        store.close().await;
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn query_filters_by_tool() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = EventStore::new(dir.path()).await?;
        let sid = SessionId::new();
        store
            .log(&Event::new(sid.clone(), "hook", "tool_a", Decision::Pass))
            .await?;
        store
            .log(&Event::new(sid, "hook", "tool_b", Decision::Pass))
            .await?;
        let results = store
            .query(&EventFilters {
                tool: Some("tool_a".to_string()),
                ..Default::default()
            })
            .await?;
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].tool, "tool_a");
        store.close().await;
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn query_filters_by_session_id() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = EventStore::new(dir.path()).await?;
        let sid1 = SessionId::new();
        let sid2 = SessionId::new();
        store
            .log(&Event::new(sid1.clone(), "hook", "tool", Decision::Pass))
            .await?;
        store
            .log(&Event::new(sid2, "hook", "tool", Decision::Pass))
            .await?;
        let results = store
            .query(&EventFilters {
                session_id: Some(sid1.clone()),
                ..Default::default()
            })
            .await?;
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].session_id, sid1);
        store.close().await;
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn query_respects_limit() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = EventStore::new(dir.path()).await?;
        for _ in 0..5 {
            store.log(&make_event("hook", Decision::Pass)).await?;
        }
        let results = store
            .query(&EventFilters {
                limit: Some(3),
                ..Default::default()
            })
            .await?;
        assert_eq!(results.len(), 3);
        store.close().await;
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn persist_rule_scan_logs_one_event_per_violation_under_scan_session(
    ) -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = EventStore::new(dir.path()).await?;
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
        let session_id = store
            .persist_rule_scan(Path::new("/tmp/project"), &violations)
            .await;
        let events = store.query(&EventFilters::default()).await?;
        assert_eq!(events.len(), 3);
        assert_eq!(events.iter().filter(|e| e.hook == "rule_scan").count(), 1);
        let check_events: Vec<_> = events.iter().filter(|e| e.hook == "rule_check").collect();
        assert_eq!(check_events.len(), 2);
        assert!(check_events
            .iter()
            .all(|event| event.session_id == session_id));
        store.close().await;
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn persist_rule_scan_logs_summary_even_when_empty() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = EventStore::new(dir.path()).await?;
        store
            .persist_rule_scan(Path::new("/tmp/project"), &[])
            .await;
        let scan_events = store
            .query(&EventFilters {
                hook: Some("rule_scan".to_string()),
                ..Default::default()
            })
            .await?;
        assert_eq!(scan_events.len(), 1);
        assert_eq!(scan_events[0].decision, Decision::Pass);

        let violation_events = store
            .query(&EventFilters {
                hook: Some("rule_check".to_string()),
                ..Default::default()
            })
            .await?;
        assert!(violation_events.is_empty());
        store.close().await;
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn persist_rule_scan_maps_severity_to_decision() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = EventStore::new(dir.path()).await?;
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
        store
            .persist_rule_scan(Path::new("/tmp/project"), &violations)
            .await;
        let events = store
            .query(&EventFilters {
                hook: Some("rule_check".to_string()),
                ..Default::default()
            })
            .await?;
        assert_eq!(events.len(), 4);
        let by_tool: std::collections::HashMap<_, _> = events
            .iter()
            .map(|e| (e.tool.as_str(), e.decision))
            .collect();
        assert_eq!(by_tool["R-CRIT"], Decision::Block);
        assert_eq!(by_tool["R-HIGH"], Decision::Block);
        assert_eq!(by_tool["R-MED"], Decision::Warn);
        assert_eq!(by_tool["R-LOW"], Decision::Pass);
        store.close().await;
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn persist_rule_scan_stores_project_path_on_anchor() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = EventStore::new(dir.path()).await?;
        let project_root = Path::new("/tmp/my-project");
        store.persist_rule_scan(project_root, &[]).await;
        let events = store
            .query(&EventFilters {
                hook: Some("rule_scan".to_string()),
                ..Default::default()
            })
            .await?;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].detail.as_deref(), Some("/tmp/my-project"));
        store.close().await;
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn log_auto_fix_report_emits_summary_and_attempt_events() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = EventStore::new(dir.path()).await?;
        let session_id = SessionId::new();

        let report = AutoFixReport {
            attempts: vec![
                AutoFixAttempt {
                    rule_id: RuleId::from_str("FIX-01"),
                    file: std::path::PathBuf::from("src/main.rs"),
                    line: Some(5),
                    applied: true,
                    resolved: true,
                },
                AutoFixAttempt {
                    rule_id: RuleId::from_str("FIX-02"),
                    file: std::path::PathBuf::from("src/lib.rs"),
                    line: None,
                    applied: false,
                    resolved: false,
                },
            ],
            fixed_count: 1,
            residual_violations: vec![],
        };

        store
            .log_auto_fix_report(&session_id, &report, Path::new("/tmp/project"))
            .await;

        let all_events = store
            .query(&EventFilters {
                session_id: Some(session_id.clone()),
                ..Default::default()
            })
            .await?;
        // 1 summary + 2 attempts = 3 events
        assert_eq!(all_events.len(), 3);

        let summary: Vec<_> = all_events.iter().filter(|e| e.hook == "auto_fix").collect();
        assert_eq!(summary.len(), 1);
        assert_eq!(summary[0].decision, Decision::Pass, "no residual = Pass");
        assert_eq!(summary[0].detail.as_deref(), Some("/tmp/project"));

        let attempts: Vec<_> = all_events
            .iter()
            .filter(|e| e.hook == "auto_fix_attempt")
            .collect();
        assert_eq!(attempts.len(), 2);

        let resolved_evt = attempts
            .iter()
            .find(|e| e.tool == "FIX-01")
            .ok_or_else(|| anyhow::anyhow!("FIX-01 attempt event not found"))?;
        assert_eq!(resolved_evt.decision, Decision::Pass);

        let unresolved_evt = attempts
            .iter()
            .find(|e| e.tool == "FIX-02")
            .ok_or_else(|| anyhow::anyhow!("FIX-02 attempt event not found"))?;
        assert_eq!(unresolved_evt.decision, Decision::Block);
        store.close().await;
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn log_auto_fix_report_summary_warns_when_residual_violations_remain(
    ) -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = EventStore::new(dir.path()).await?;
        let session_id = SessionId::new();

        let report = AutoFixReport {
            attempts: vec![],
            fixed_count: 0,
            residual_violations: vec![Violation {
                rule_id: RuleId::from_str("R-01"),
                file: std::path::PathBuf::from("src/main.rs"),
                line: None,
                message: "still broken".to_string(),
                severity: Severity::High,
            }],
        };

        store
            .log_auto_fix_report(&session_id, &report, Path::new("/tmp/project"))
            .await;

        let events = store
            .query(&EventFilters {
                hook: Some("auto_fix".to_string()),
                ..Default::default()
            })
            .await?;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].decision, Decision::Warn);
        store.close().await;
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn log_external_signal_and_query_roundtrip() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = EventStore::new(dir.path()).await?;
        let signal = ExternalSignal::new(
            "github".to_string(),
            Severity::High,
            serde_json::json!({"action": "completed", "conclusion": "failure"}),
        );
        store.log_external_signal(&signal)?;
        let results = store.query_external_signals(None)?;
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, signal.id);
        assert_eq!(results[0].source, "github");
        store.close().await;
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn query_external_signals_filters_by_since() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = EventStore::new(dir.path()).await?;
        let old_signal =
            ExternalSignal::new("github".to_string(), Severity::Low, serde_json::json!({}));
        store.log_external_signal(&old_signal)?;
        let cutoff = chrono::Utc::now();
        let new_signal = ExternalSignal::new(
            "github".to_string(),
            Severity::High,
            serde_json::json!({"conclusion": "failure"}),
        );
        store.log_external_signal(&new_signal)?;
        let results = store.query_external_signals(Some(cutoff))?;
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, new_signal.id);
        store.close().await;
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn query_external_signals_empty_when_no_file() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = EventStore::new(dir.path()).await?;
        let results = store.query_external_signals(None)?;
        assert!(results.is_empty());
        store.close().await;
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn log_with_unreachable_otel_endpoint_still_persists_event() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let config = OtelConfig {
            exporter: OtelExporter::OtlpHttp,
            endpoint: Some("http://127.0.0.1:1".to_string()),
            ..OtelConfig::default()
        };
        let store = EventStore::with_policies_and_otel(dir.path(), 1800, 90, &config).await?;
        assert!(store
            .otel_pipeline
            .lock()
            .expect("otel pipeline mutex should not be poisoned")
            .is_none());
        let event = Event::new(
            SessionId::new(),
            "api_request",
            "http_client",
            Decision::Pass,
        );
        store.log(&event).await?;
        let events = store
            .query(&EventFilters {
                session_id: Some(event.session_id.clone()),
                ..Default::default()
            })
            .await?;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].id, event.id);
        store.close().await;
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn migrate_from_jsonl_imports_existing_events() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        // Write a JSONL file before opening the store
        let event = make_event("pre_tool_use", Decision::Pass);
        let line = serde_json::to_string(&event)?;
        let jsonl_path = dir.path().join("events.jsonl");
        std::fs::write(&jsonl_path, format!("{line}\n"))?;

        let store = EventStore::new(dir.path()).await?;
        let results = store.query(&EventFilters::default()).await?;
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, event.id);
        store.close().await;
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn session_renewal_secs_default_is_1800() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = EventStore::new(dir.path()).await?;
        assert_eq!(store.session_renewal_secs(), 1800);
        store.close().await;
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn purge_old_events_zero_days_is_noop() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = EventStore::new(dir.path()).await?;
        store.log(&make_event("hook", Decision::Pass)).await?;
        let deleted = store.purge_old_events(0).await?;
        assert_eq!(deleted, 0);
        let results = store.query(&EventFilters::default()).await?;
        assert_eq!(results.len(), 1);
        store.close().await;
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn purge_old_events_removes_stale_and_keeps_recent() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = EventStore::new(dir.path()).await?;

        // Insert an old event (101 days ago)
        let mut old_event = make_event("hook", Decision::Pass);
        old_event.ts = chrono::Utc::now() - chrono::Duration::days(101);
        store.log(&old_event).await?;

        // Insert a recent event (1 day ago)
        let recent_event = make_event("hook", Decision::Pass);
        store.log(&recent_event).await?;

        let deleted = store.purge_old_events(90).await?;
        assert_eq!(deleted, 1, "only the old event should be purged");

        let results = store.query(&EventFilters::default()).await?;
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, recent_event.id);
        store.close().await;
        Ok(())
    }

    /// Verify that constructing an EventStore with a path containing ".." resolves
    /// to the canonical directory: the DB file must land at the canonical root, not
    /// inside any intermediate sub-directory.
    #[tokio::test(flavor = "multi_thread")]
    async fn constructor_canonicalizes_path_with_dotdot() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        // Build a path with a ".." component: <tmp>/sub/../  (resolves to <tmp>/)
        let sub = dir.path().join("sub");
        std::fs::create_dir_all(&sub)?;
        let traversal_path = sub.join("..");

        let store = EventStore::new(&traversal_path).await?;

        // The DB file must land at the canonical root, not inside "sub/".
        let canonical_root = dir.path().canonicalize()?;
        assert!(
            canonical_root.join("events.db").exists(),
            "events.db must be at canonical root {canonical_root:?}"
        );
        store.close().await;
        Ok(())
    }
}
