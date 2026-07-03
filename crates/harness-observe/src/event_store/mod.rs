use chrono::{DateTime, Utc};
use harness_core::config::misc::OtelConfig;
use harness_core::db::{pg_open_pool, PgStoreContext};
use harness_core::run_id::RunId;
use harness_core::types::{
    AutoFixReport, Decision, Event, EventFilters, EventId, ExternalSignal, ExternalSignalId, Grade,
    SessionId, Severity, Violation,
};
use sqlx::postgres::PgPool;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Mutex;

mod legacy;
mod migrations;

pub use legacy::migrate_legacy_event_store_if_needed;
use migrations::EVENT_MIGRATIONS;
pub use migrations::EVENT_STORE_SCHEMA;

/// Event store backed by Postgres.
///
/// Backward compatibility: on first startup the store imports any existing
/// `events.jsonl` file found in the data directory, then leaves it in place as
/// an archive.
pub struct EventStore {
    pool: PgPool,
    schema: String,
    store_key: String,
    data_dir: PathBuf,
    otel_pipeline: Mutex<Option<crate::otel_export::OtelPipeline>>,
    session_renewal_secs: u64,
}

impl EventStore {
    pub async fn new(data_dir: &Path) -> anyhow::Result<Self> {
        Self::new_with_database_url(data_dir, None).await
    }

    pub async fn new_with_database_url(
        data_dir: &Path,
        configured_database_url: Option<&str>,
    ) -> anyhow::Result<Self> {
        let context = Self::shared_schema_context(configured_database_url)?;
        let setup_pool = pg_open_pool(context.database_url()).await?;
        let store = Self::new_shared_with_context(data_dir, &context, &setup_pool).await;
        setup_pool.close().await;
        store
    }

    pub fn shared_schema_context(
        configured_database_url: Option<&str>,
    ) -> anyhow::Result<PgStoreContext> {
        PgStoreContext::from_schema(EVENT_STORE_SCHEMA, configured_database_url)
    }

    pub async fn new_with_context(
        data_dir: &Path,
        context: &PgStoreContext,
        setup_pool: &PgPool,
    ) -> anyhow::Result<Self> {
        let store_key = context.schema().to_owned();
        let store =
            Self::new_with_context_and_store_key(data_dir, context, setup_pool, store_key).await?;
        store.migrate_from_jsonl().await;
        Ok(store)
    }

    pub async fn new_shared_with_context(
        data_dir: &Path,
        context: &PgStoreContext,
        setup_pool: &PgPool,
    ) -> anyhow::Result<Self> {
        std::fs::create_dir_all(data_dir)?;
        let store_key = Self::store_key_for_data_dir(data_dir)?;
        let store =
            Self::new_with_context_and_store_key(data_dir, context, setup_pool, store_key).await?;
        migrate_legacy_event_store_if_needed(
            &data_dir.join("events.db"),
            Some(context.database_url()),
            &store,
        )
        .await?;
        store.migrate_from_jsonl().await;
        Ok(store)
    }

    async fn new_with_context_and_store_key(
        data_dir: &Path,
        context: &PgStoreContext,
        setup_pool: &PgPool,
        store_key: String,
    ) -> anyhow::Result<Self> {
        std::fs::create_dir_all(data_dir)?;
        let pool = context
            .open_migrated_pool_with_setup_pool(setup_pool, EVENT_MIGRATIONS)
            .await?;
        Ok(Self {
            pool,
            schema: context.schema().to_owned(),
            store_key,
            data_dir: data_dir.to_path_buf(),
            otel_pipeline: Mutex::new(None),
            session_renewal_secs: 1800,
        })
    }

    pub fn store_key_for_data_dir(data_dir: &Path) -> anyhow::Result<String> {
        let canonical = data_dir.canonicalize().map_err(|error| {
            anyhow::anyhow!(
                "failed to canonicalize event store data_dir {}: {error}",
                data_dir.display()
            )
        })?;
        Ok(canonical.to_string_lossy().into_owned())
    }

    pub fn schema(&self) -> &str {
        &self.schema
    }

    pub fn store_key(&self) -> &str {
        &self.store_key
    }

    /// Close the connection pool.
    pub async fn close(self) {
        self.pool.close().await;
    }

    /// Create a non-functional store for unit tests that need an `&EventStore`
    /// but do not care about event persistence. All `log` calls will fail and
    /// callers that handle those errors (e.g. with `tracing::warn!`) will
    /// continue normally. Do not use outside of test code.
    #[doc(hidden)]
    pub fn new_noop_for_tests() -> Self {
        let pool = match PgPool::connect_lazy("postgresql://localhost/harness_noop") {
            Ok(pool) => pool,
            Err(error) => panic!("lazy pool URL must be syntactically valid: {error}"),
        };
        Self {
            pool,
            schema: String::new(),
            store_key: String::new(),
            data_dir: PathBuf::new(),
            otel_pipeline: Mutex::new(None),
            session_renewal_secs: 1800,
        }
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
                "DELETE FROM events WHERE store_key = $1 AND id IN (
                    SELECT id FROM events
                    WHERE store_key = $1
                      AND ts < $2
                      AND hook NOT LIKE 'periodic_review:%'
                      AND hook NOT LIKE 'periodic_retry:%'
                    LIMIT 500
                )",
            )
            .bind(&self.store_key)
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
                "DELETE FROM events WHERE store_key = $1 AND id IN (
                    SELECT e.id FROM events e
                    WHERE e.store_key = $1
                    AND (e.hook LIKE 'periodic_review:%' OR e.hook = 'periodic_retry:summary')
                    AND e.ts < (
                        SELECT MAX(e2.ts) FROM events e2
                        WHERE e2.store_key = $1 AND e2.hook = e.hook
                    )
                    LIMIT 500
                )",
            )
            .bind(&self.store_key)
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
        Self::with_policies_and_otel_with_database_url(
            data_dir,
            None,
            session_renewal_secs,
            log_retention_days,
            otel_config,
        )
        .await
    }

    pub async fn with_policies_and_otel_with_database_url(
        data_dir: &Path,
        configured_database_url: Option<&str>,
        session_renewal_secs: u64,
        log_retention_days: u32,
        otel_config: &OtelConfig,
    ) -> anyhow::Result<Self> {
        let mut store = Self::new_with_database_url(data_dir, configured_database_url).await?;
        store
            .apply_policies_and_otel(session_renewal_secs, log_retention_days, otel_config)
            .await?;
        Ok(store)
    }

    pub async fn with_policies_and_otel_with_context(
        data_dir: &Path,
        context: &PgStoreContext,
        setup_pool: &PgPool,
        session_renewal_secs: u64,
        log_retention_days: u32,
        otel_config: &OtelConfig,
    ) -> anyhow::Result<Self> {
        let mut store = Self::new_with_context(data_dir, context, setup_pool).await?;
        store
            .apply_policies_and_otel(session_renewal_secs, log_retention_days, otel_config)
            .await?;
        Ok(store)
    }

    pub async fn with_policies_and_otel_shared_with_context(
        data_dir: &Path,
        context: &PgStoreContext,
        setup_pool: &PgPool,
        session_renewal_secs: u64,
        log_retention_days: u32,
        otel_config: &OtelConfig,
    ) -> anyhow::Result<Self> {
        let mut store = Self::new_shared_with_context(data_dir, context, setup_pool).await?;
        store
            .apply_policies_and_otel(session_renewal_secs, log_retention_days, otel_config)
            .await?;
        Ok(store)
    }

    async fn apply_policies_and_otel(
        &mut self,
        session_renewal_secs: u64,
        log_retention_days: u32,
        otel_config: &OtelConfig,
    ) -> anyhow::Result<()> {
        self.session_renewal_secs = session_renewal_secs;
        tracing::debug!(
            session_renewal_secs,
            log_retention_days,
            "event store: applying retention policies"
        );
        if let Err(e) = self.purge_old_events(log_retention_days).await {
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
        *self.otel_pipeline.lock().unwrap_or_else(|e| e.into_inner()) = pipeline;
        Ok(())
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

        // Fast-path only for this data directory. Other store keys in the
        // shared schema must not suppress this store's legacy import.
        match sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM events WHERE store_key = $1")
            .bind(&self.store_key)
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
        let metadata = match &event.metadata {
            Some(metadata) => Some(serde_json::to_string(metadata)?),
            None => None,
        };
        sqlx::query(
            "INSERT INTO events
                (store_key, id, ts, session_id, run_id, hook, tool, decision, reason, detail, duration_ms, content, metadata)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)
             ON CONFLICT (store_key, id) DO NOTHING",
        )
        .bind(&self.store_key)
        .bind(event.id.as_str())
        .bind(&ts)
        .bind(event.session_id.as_str())
        .bind(event.run_id.as_ref().map(RunId::as_str))
        .bind(&event.hook)
        .bind(&event.tool)
        .bind(decision)
        .bind(&event.reason)
        .bind(&event.detail)
        .bind(event.duration_ms.map(|v| v as i64))
        .bind(&event.content)
        .bind(metadata)
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
            "SELECT id, ts, session_id, run_id, hook, tool, decision, reason, detail, duration_ms, {content_col}, metadata
             FROM events WHERE store_key = $1",
        );
        let mut param_count = 1usize;
        if filters.session_id.is_some() {
            param_count += 1;
            sql.push_str(&format!(" AND session_id = ${param_count}"));
        }
        if filters.run_id.is_some() {
            param_count += 1;
            sql.push_str(&format!(" AND run_id = ${param_count}"));
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

        let mut q = sqlx::query(&sql).bind(&self.store_key);

        if let Some(ref sid) = filters.session_id {
            q = q.bind(sid.as_str());
        }
        if let Some(ref run_id) = filters.run_id {
            q = q.bind(run_id.as_str());
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
        let run_id: Option<String> = row.try_get("run_id")?;
        let hook: String = row.try_get("hook")?;
        let tool: String = row.try_get("tool")?;
        let decision_str: String = row.try_get("decision")?;
        let reason: Option<String> = row.try_get("reason")?;
        let detail: Option<String> = row.try_get("detail")?;
        let content: Option<String> = row.try_get("content")?;
        let metadata_json: Option<String> = row.try_get("metadata")?;
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
            run_id: run_id.as_deref().map(RunId::from_str).transpose()?,
            hook,
            tool,
            decision,
            reason,
            detail,
            content,
            metadata: metadata_json
                .as_deref()
                .map(serde_json::from_str)
                .transpose()?,
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
            "SELECT last_scan_ts FROM scan_watermarks
             WHERE store_key = $1 AND project = $2 AND agent_id = $3",
        )
        .bind(&self.store_key)
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
            "INSERT INTO scan_watermarks (store_key, project, agent_id, last_scan_ts)
             VALUES ($1, $2, $3, $4)
             ON CONFLICT (store_key, project, agent_id)
             DO UPDATE SET last_scan_ts = EXCLUDED.last_scan_ts",
        )
        .bind(&self.store_key)
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
}

#[cfg(test)]
mod shared_schema_tests;

#[cfg(test)]
mod store_tests;
