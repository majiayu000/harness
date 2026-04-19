use chrono::{DateTime, Utc};
use harness_core::config::misc::OtelConfig;
use harness_core::db::{pg_open_pool, pg_open_pool_schematized, Migration, PgMigrator};
use harness_core::types::{
    AutoFixReport, Decision, Event, EventFilters, EventId, ExternalSignal, ExternalSignalId, Grade,
    SessionId, Severity, Violation,
};
use sqlx::postgres::PgPool;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

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
        duration_ms BIGINT
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
    Migration {
        version: 3,
        description: "create scan_watermarks table",
        sql: "CREATE TABLE IF NOT EXISTS scan_watermarks (
            project      TEXT NOT NULL,
            agent_id     TEXT NOT NULL,
            last_scan_ts TEXT NOT NULL,
            PRIMARY KEY (project, agent_id)
        )",
    },
];

/// Event store backed by Postgres.
///
/// Backward compatibility: on first startup the store imports any existing
/// `events.jsonl` file found in the data directory, then leaves it in place as
/// an archive.
pub struct EventStore {
    pool: PgPool,
    data_dir: PathBuf,
    otel_pipeline: Mutex<Option<crate::otel_export::OtelPipeline>>,
    session_renewal_secs: u64,
}

impl EventStore {
    pub async fn new(data_dir: &Path) -> anyhow::Result<Self> {
        std::fs::create_dir_all(data_dir)?;
        let data_dir = data_dir.to_path_buf();

        let database_url = std::env::var("DATABASE_URL")
            .map_err(|_| anyhow::anyhow!("DATABASE_URL environment variable is not set"))?;
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        data_dir.join("events.db").hash(&mut hasher);
        let schema = format!("h{:016x}", hasher.finish());

        let setup = pg_open_pool(&database_url).await?;
        sqlx::query(&format!("CREATE SCHEMA IF NOT EXISTS \"{}\"", schema))
            .execute(&setup)
            .await?;
        drop(setup);

        let pool = pg_open_pool_schematized(&database_url, &schema).await?;
        PgMigrator::new(&pool, EVENT_MIGRATIONS).run().await?;

        let store = Self {
            pool,
            data_dir,
            otel_pipeline: Mutex::new(None),
            session_renewal_secs: 1800,
        };

        store.migrate_from_jsonl().await;
        Ok(store)
    }

    /// Close the connection pool.
    pub async fn close(self) {
        self.pool.close().await;
    }

    pub fn session_renewal_secs(&self) -> u64 {
        self.session_renewal_secs
    }

    /// Delete all events whose timestamp is older than `days` days.
    ///
    /// Phase 1: deletes regular events older than the retention window, sparing
    /// periodic_review:* and periodic_retry:* hooks (used as watermark cursors).
    /// Phase 2: keeps only the newest row per watermark hook.
    ///
    /// Returns the number of rows deleted.  A `days` value of 0 is a no-op.
    pub async fn purge_old_events(&self, days: u32) -> anyhow::Result<u64> {
        if days == 0 {
            return Ok(0);
        }
        let cutoff = chrono::Utc::now() - chrono::Duration::days(i64::from(days));
        let cutoff_str = cutoff.to_rfc3339();
        let mut total_deleted: u64 = 0;

        loop {
            let result = sqlx::query(
                "DELETE FROM events WHERE id IN (
                    SELECT id FROM events
                    WHERE ts < $1
                      AND hook NOT LIKE 'periodic_review:%'
                      AND hook NOT LIKE 'periodic_retry:%'
                    LIMIT 500
                )",
            )
            .bind(&cutoff_str)
            .execute(&self.pool)
            .await?;
            let batch = result.rows_affected();
            total_deleted += batch;
            if batch == 0 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        loop {
            let result = sqlx::query(
                "DELETE FROM events WHERE id IN (
                    SELECT e.id FROM events e
                    WHERE (e.hook LIKE 'periodic_review:%' OR e.hook = 'periodic_retry:summary')
                    AND e.ts < (
                        SELECT MAX(e2.ts) FROM events e2 WHERE e2.hook = e.hook
                    )
                    LIMIT 500
                )",
            )
            .execute(&self.pool)
            .await?;
            let batch = result.rows_affected();
            total_deleted += batch;
            if batch == 0 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        if total_deleted > 0 {
            tracing::info!(
                deleted = total_deleted,
                days,
                "event store: purged old events"
            );
        }
        Ok(total_deleted)
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
        *store
            .otel_pipeline
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = pipeline;
        Ok(store)
    }

    /// Import events from an existing `events.jsonl` file (backward compat).
    ///
    /// Fast-paths out when the `events` table already contains rows: on each
    /// startup we previously re-read the full JSONL and fired one `INSERT
    /// ... ON CONFLICT DO NOTHING` per line. Against a remote Postgres (e.g.
    /// Supabase session pooler, ~1s RTT per statement) this added ~N seconds
    /// to every server boot for N lines in the file, even though every row
    /// was already in the DB. Checking the table once is a single query.
    async fn migrate_from_jsonl(&self) {
        use std::io::BufRead as _;

        // Fast-path: if any events are already in the DB, assume a prior
        // successful migration and skip the per-line replay entirely.
        match sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM events")
            .fetch_one(&self.pool)
            .await
        {
            Ok(n) if n > 0 => return,
            Ok(_) => {}
            Err(e) => {
                tracing::warn!(
                    "event store: could not check existing event count, will still attempt JSONL migration: {e}"
                );
            }
        }

        let path = self.data_dir.join("events.jsonl");
        let file = match std::fs::File::open(&path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return;
            }
            Err(e) => {
                tracing::warn!("event store: could not open events.jsonl for migration: {e}");
                return;
            }
        };
        let mut imported = 0usize;
        for line in std::io::BufReader::new(file).lines() {
            let line = match line {
                Ok(l) => l,
                Err(e) => {
                    tracing::warn!(
                        "event store: I/O error reading events.jsonl, aborting migration: {e}"
                    );
                    break;
                }
            };
            let line = line.trim().to_owned();
            if line.is_empty() {
                continue;
            }
            if let Ok(event) = serde_json::from_str::<Event>(&line) {
                if let Err(e) = self.insert_event(&event).await {
                    tracing::warn!("event store: failed to insert migrated event: {e}");
                } else {
                    imported += 1;
                }
            }
        }
        if imported > 0 {
            tracing::info!(imported, "event store: migrated events from events.jsonl");
        }
    }

    async fn insert_event(&self, event: &Event) -> anyhow::Result<()> {
        let decision = serde_json::to_string(&event.decision)?;
        let decision = decision.trim_matches('"');
        let ts = event.ts.to_rfc3339();
        sqlx::query(
            "INSERT INTO events
                (id, ts, session_id, hook, tool, decision, reason, detail, duration_ms, content)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
             ON CONFLICT (id) DO NOTHING",
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
        let slot = self.otel_pipeline.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(pipeline) = slot.as_ref() {
            pipeline.record_event(event);
        }
        Ok(event.id.clone())
    }

    pub async fn query(&self, filters: &EventFilters) -> anyhow::Result<Vec<Event>> {
        let content_col = if filters.include_content {
            "content"
        } else {
            "NULL as content"
        };
        let mut sql = format!(
            "SELECT id, ts, session_id, hook, tool, decision, reason, detail, duration_ms, {content_col}
             FROM events WHERE 1=1",
        );
        let mut param_count = 0usize;
        if filters.session_id.is_some() {
            param_count += 1;
            sql.push_str(&format!(" AND session_id = ${param_count}"));
        }
        if filters.hook.is_some() {
            param_count += 1;
            sql.push_str(&format!(" AND hook = ${param_count}"));
        }
        if filters.tool.is_some() {
            param_count += 1;
            sql.push_str(&format!(" AND tool = ${param_count}"));
        }
        if filters.decision.is_some() {
            param_count += 1;
            sql.push_str(&format!(" AND decision = ${param_count}"));
        }
        if filters.since.is_some() {
            param_count += 1;
            sql.push_str(&format!(" AND ts >= ${param_count}"));
        }
        if filters.until.is_some() {
            param_count += 1;
            sql.push_str(&format!(" AND ts <= ${param_count}"));
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

    fn row_to_event(row: &sqlx::postgres::PgRow) -> anyhow::Result<Event> {
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
        let pipeline = self
            .otel_pipeline
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        if let Some(pipeline) = pipeline {
            pipeline.shutdown().await;
        }
    }

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

    pub async fn persist_retry_summary(
        &self,
        checked: u32,
        retried: u32,
        stuck: u32,
        skipped: u32,
    ) {
        let decision = if stuck > 0 {
            Decision::Warn
        } else {
            Decision::Pass
        };
        let mut event = Event::new(
            SessionId::new(),
            "periodic_retry:summary",
            "RetryScheduler",
            decision,
        );
        event.detail = Some(format!(
            r#"{{"checked":{checked},"retried":{retried},"stuck":{stuck},"skipped":{skipped}}}"#
        ));
        if let Err(e) = self.log(&event).await {
            tracing::warn!("periodic_retry: failed to log summary event: {e}");
        }
    }

    pub async fn get_scan_watermark(
        &self,
        project: &str,
        agent_id: &str,
    ) -> anyhow::Result<Option<DateTime<Utc>>> {
        let row: Option<(String,)> = sqlx::query_as(
            "SELECT last_scan_ts FROM scan_watermarks WHERE project = $1 AND agent_id = $2",
        )
        .bind(project)
        .bind(agent_id)
        .fetch_optional(&self.pool)
        .await?;
        match row {
            None => Ok(None),
            Some((ts_str,)) => {
                let ts = chrono::DateTime::parse_from_rfc3339(&ts_str)
                    .map_err(|e| anyhow::anyhow!("invalid watermark ts '{ts_str}': {e}"))?
                    .with_timezone(&chrono::Utc);
                Ok(Some(ts))
            }
        }
    }

    pub async fn set_scan_watermark(
        &self,
        project: &str,
        agent_id: &str,
        ts: DateTime<Utc>,
    ) -> anyhow::Result<()> {
        sqlx::query(
            "INSERT INTO scan_watermarks (project, agent_id, last_scan_ts)
             VALUES ($1, $2, $3)
             ON CONFLICT (project, agent_id) DO UPDATE SET last_scan_ts = EXCLUDED.last_scan_ts",
        )
        .bind(project)
        .bind(agent_id)
        .bind(ts.to_rfc3339())
        .execute(&self.pool)
        .await?;
        Ok(())
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

    #[cfg(test)]
    pub(crate) fn otel_pipeline_is_none(&self) -> bool {
        self.otel_pipeline
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_none()
    }

    /// Test-only variant of with_policies_and_otel using small pool sizes.
    #[cfg(test)]
    pub(crate) async fn with_policies_and_otel_for_test(
        data_dir: &Path,
        session_renewal_secs: u64,
        log_retention_days: u32,
        otel_config: &OtelConfig,
    ) -> anyhow::Result<Self> {
        let mut store = Self::new_for_test(data_dir).await?;
        store.session_renewal_secs = session_renewal_secs;
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
        *store
            .otel_pipeline
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = pipeline;
        Ok(store)
    }

    /// Test-only constructor using max_connections(2) to prevent pool exhaustion
    /// when many tests run in parallel against a shared Postgres instance.
    #[cfg(test)]
    pub(crate) async fn new_for_test(data_dir: &Path) -> anyhow::Result<Self> {
        use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
        use std::str::FromStr as _;

        std::fs::create_dir_all(data_dir)?;
        let data_dir = data_dir.to_path_buf();

        let database_url = std::env::var("DATABASE_URL")
            .map_err(|_| anyhow::anyhow!("DATABASE_URL environment variable is not set"))?;
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        data_dir.join("events.db").hash(&mut hasher);
        let schema = format!("h{:016x}", hasher.finish());

        let setup = PgPoolOptions::new()
            .max_connections(2)
            .acquire_timeout(std::time::Duration::from_secs(30))
            .connect(&database_url)
            .await?;
        sqlx::query(&format!("CREATE SCHEMA IF NOT EXISTS \"{}\"", schema))
            .execute(&setup)
            .await?;
        drop(setup);

        let schema_for_hook = schema.clone();
        let opts =
            PgConnectOptions::from_str(&database_url)?.options([("search_path", schema.as_str())]);
        let pool = PgPoolOptions::new()
            .max_connections(2)
            .acquire_timeout(std::time::Duration::from_secs(30))
            .after_connect(move |conn, _meta| {
                let schema = schema_for_hook.clone();
                Box::pin(async move {
                    let stmt = format!("SET search_path TO \"{}\"", schema);
                    sqlx::query(&stmt).execute(conn).await?;
                    Ok(())
                })
            })
            .connect_with(opts)
            .await?;
        PgMigrator::new(&pool, EVENT_MIGRATIONS).run().await?;

        let store = Self {
            pool,
            data_dir,
            otel_pipeline: Mutex::new(None),
            session_renewal_secs: 1800,
        };
        store.migrate_from_jsonl().await;
        Ok(store)
    }
}

#[cfg(test)]
mod store_tests;
