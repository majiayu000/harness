use chrono::Utc;
use harness_core::db::{open_pool, rewrite_placeholders, Dialect, Migration, Migrator};
use serde::{Deserialize, Serialize};
use sqlx::AnyPool;
use std::path::Path;

type FindingRow = (
    String,
    String,
    String,
    i32,
    i32,
    i32,
    String,
    i64,
    String,
    String,
    String,
    Option<String>,
);

/// A single finding from a periodic review.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewFinding {
    pub id: String,
    pub rule_id: String,
    pub priority: String,
    pub impact: i32,
    pub confidence: i32,
    pub effort: i32,
    pub file: String,
    pub line: i64,
    pub title: String,
    pub description: String,
    pub action: String,
    /// Task ID of the auto-spawned fix task, if any.
    #[serde(default)]
    pub task_id: Option<String>,
}

/// Summary scores from a review run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewSummary {
    pub p0_count: i32,
    pub p1_count: i32,
    pub p2_count: i32,
    pub p3_count: i32,
    pub health_score: i32,
}

/// Complete structured review output from the agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewOutput {
    pub findings: Vec<ReviewFinding>,
    pub summary: ReviewSummary,
}

static REVIEW_MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        description: "create review_findings table",
        sql: "CREATE TABLE IF NOT EXISTS review_findings (
            id          TEXT NOT NULL,
            review_id   TEXT NOT NULL,
            rule_id     TEXT NOT NULL,
            priority    TEXT NOT NULL,
            impact      INTEGER NOT NULL,
            confidence  INTEGER NOT NULL,
            effort      INTEGER NOT NULL,
            file        TEXT NOT NULL,
            line        INTEGER NOT NULL DEFAULT 0,
            title       TEXT NOT NULL,
            description TEXT NOT NULL,
            action      TEXT NOT NULL,
            status      TEXT NOT NULL DEFAULT 'open',
            created_at  TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
            task_id     TEXT,
            PRIMARY KEY (review_id, id)
        )",
    },
    Migration {
        version: 2,
        description: "add claimed_at column and backfill stuck pending rows",
        // Guard: add task_id first so the subsequent UPDATE reference is safe on
        // legacy tables that predate the task_id column.  duplicate_add_column_error
        // silently swallows the "duplicate column name" error if it already exists.
        sql: "ALTER TABLE review_findings ADD COLUMN task_id TEXT;
              ALTER TABLE review_findings ADD COLUMN claimed_at TEXT;
              UPDATE review_findings
              SET claimed_at = CURRENT_TIMESTAMP
              WHERE task_id = 'pending' AND claimed_at IS NULL",
    },
    Migration {
        version: 3,
        description: "add real_task_id column",
        sql: "ALTER TABLE review_findings ADD COLUMN real_task_id TEXT",
    },
    Migration {
        version: 4,
        description: "deduplicate findings and add unique index",
        // ORDER BY preserves whichever duplicate owns a spawned task:
        //   0 = real_task_id set (task enqueued — highest priority to keep)
        //   1 = task_id set but no real_task_id (claim in-flight)
        //   2 = unclaimed row
        // Within the same ownership tier, prefer the newest created_at.
        // Without this ordering a plain "ORDER BY created_at DESC" could delete
        // the older row that already owns a spawned task and keep a newer
        // unclaimed duplicate, causing the scheduler to re-spawn the fix task.
        sql: "WITH ranked AS (
            SELECT review_id, id,
                   ROW_NUMBER() OVER (
                       PARTITION BY rule_id, file, status
                       ORDER BY
                           CASE WHEN real_task_id IS NOT NULL THEN 0
                                WHEN task_id IS NOT NULL THEN 1
                                ELSE 2 END ASC,
                           created_at DESC
                   ) AS rn
            FROM review_findings
        )
        DELETE FROM review_findings
        WHERE (review_id, id) IN (SELECT review_id, id FROM ranked WHERE rn > 1);
        CREATE UNIQUE INDEX IF NOT EXISTS idx_finding_dedup
        ON review_findings(rule_id, file, status)",
    },
    Migration {
        version: 5,
        description: "normalize legacy claimed_at timestamps to RFC3339",
        // Migration v2 backfilled claimed_at using SQL CURRENT_TIMESTAMP which
        // produces "YYYY-MM-DD HH:MM:SS" (space separator, length 19).
        // recover_stale_pending_claims compares claimed_at against an RFC3339
        // cutoff string "YYYY-MM-DDTHH:MM:SSZ" (T separator).  Because space
        // (0x20) < 'T' (0x54) in ASCII, every space-separated value sorts before
        // every T-separated value on the same date, making all v2-backfilled rows
        // appear permanently stale immediately after upgrade.
        //
        // Fix: detect rows in the old format (length=19, space at position 11)
        // and rewrite them in-place to RFC3339 by inserting 'T' and appending 'Z',
        // preserving the original wall-clock time so stale_secs still applies.
        //
        // substr() with 1-based indexing and || string concatenation work on both
        // SQLite and Postgres.
        sql: "UPDATE review_findings
              SET claimed_at = substr(claimed_at, 1, 10) || 'T' || substr(claimed_at, 12) || 'Z'
              WHERE claimed_at IS NOT NULL
                AND length(claimed_at) = 19
                AND substr(claimed_at, 11, 1) = ' '",
    },
];

/// Persists review findings to SQLite.
pub struct ReviewStore {
    pool: AnyPool,
    dialect: Dialect,
}

impl ReviewStore {
    pub async fn open(db_path: &Path) -> anyhow::Result<Self> {
        let pool = open_pool(db_path).await?;
        Migrator::new(&pool, REVIEW_MIGRATIONS).run().await?;
        Ok(Self {
            pool,
            dialect: Dialect::Sqlite,
        })
    }

    /// Persist findings from a review run, deduplicating against existing open findings.
    ///
    /// Same-batch PK duplicates (same id within one review) are caught eagerly and
    /// returned as an error rather than silently dropped by INSERT ... ON CONFLICT DO NOTHING.
    /// Cross-review dedup (same rule_id+file already open) is handled atomically by
    /// INSERT ... ON CONFLICT DO NOTHING + conditional UPDATE, eliminating the TOCTOU race.
    pub async fn persist_findings(
        &self,
        review_id: &str,
        findings: &[ReviewFinding],
    ) -> anyhow::Result<usize> {
        // Detect PK duplicates within the batch before touching the DB.
        // INSERT ... ON CONFLICT DO NOTHING would silently drop them; surface an explicit error instead.
        let mut seen_ids = std::collections::HashSet::with_capacity(findings.len());
        for f in findings {
            if !seen_ids.insert(f.id.as_str()) {
                anyhow::bail!(
                    "duplicate finding id '{}' in review '{}' batch",
                    f.id,
                    review_id
                );
            }
        }
        let mut tx = self.pool.begin().await?;
        let mut inserted = 0;
        let insert_sql = rewrite_placeholders(
            "INSERT INTO review_findings \
             (id, review_id, rule_id, priority, impact, confidence, effort, \
              file, line, title, description, action) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) ON CONFLICT DO NOTHING",
            self.dialect,
        );
        let update_sql = rewrite_placeholders(
            "UPDATE review_findings SET review_id = ? \
             WHERE rule_id = ? AND file = ? AND status = 'open'",
            self.dialect,
        );
        for f in findings {
            let result = sqlx::query(&insert_sql)
                .bind(&f.id)
                .bind(review_id)
                .bind(&f.rule_id)
                .bind(&f.priority)
                .bind(f.impact)
                .bind(f.confidence)
                .bind(f.effort)
                .bind(&f.file)
                .bind(f.line)
                .bind(&f.title)
                .bind(&f.description)
                .bind(&f.action)
                .execute(&mut *tx)
                .await?;

            if result.rows_affected() == 1 {
                inserted += 1;
            } else {
                // Existing open finding for the same rule_id+file — mark as recurring.
                sqlx::query(&update_sql)
                    .bind(review_id)
                    .bind(&f.rule_id)
                    .bind(&f.file)
                    .execute(&mut *tx)
                    .await?;
            }
        }
        tx.commit().await?;
        Ok(inserted)
    }

    /// List all open findings, ordered by priority then impact.
    pub async fn list_open(&self) -> anyhow::Result<Vec<ReviewFinding>> {
        let sql = rewrite_placeholders(
            "SELECT id, rule_id, priority, impact, confidence, effort, \
                    file, line, title, description, action, task_id \
             FROM review_findings WHERE status = 'open' \
             ORDER BY \
               CASE priority WHEN 'P0' THEN 0 WHEN 'P1' THEN 1 \
                             WHEN 'P2' THEN 2 WHEN 'P3' THEN 3 ELSE 4 END, \
               impact DESC",
            self.dialect,
        );
        let rows: Vec<FindingRow> = sqlx::query_as(&sql).fetch_all(&self.pool).await?;

        Ok(Self::rows_to_findings(rows))
    }

    /// List open findings eligible for auto-spawn: matching the given priorities,
    /// status='open', and no existing task_id (dedup guard).
    ///
    /// P0 is excluded intentionally: critical issues require human judgment;
    /// auto-fix could cause high blast-radius changes.
    /// P3 is excluded intentionally: informational only, too low priority.
    pub async fn list_spawnable_findings(
        &self,
        review_id: &str,
        priorities: &[&str],
    ) -> anyhow::Result<Vec<ReviewFinding>> {
        if priorities.is_empty() {
            return Ok(vec![]);
        }
        let placeholders = priorities
            .iter()
            .map(|_| "?")
            .collect::<Vec<_>>()
            .join(", ");
        let raw = format!(
            "SELECT id, rule_id, priority, impact, confidence, effort, \
                    file, line, title, description, action, task_id \
             FROM review_findings \
             WHERE review_id = ? AND priority IN ({placeholders}) \
               AND status = 'open' AND task_id IS NULL"
        );
        let sql = rewrite_placeholders(&raw, self.dialect);
        let mut query = sqlx::query_as::<_, FindingRow>(&sql).bind(review_id.to_owned());
        for p in priorities {
            query = query.bind(p.to_string());
        }
        let rows = query.fetch_all(&self.pool).await?;
        Ok(Self::rows_to_findings(rows))
    }

    /// Atomically claim a finding for auto-spawn by setting task_id to "pending".
    ///
    /// Uses `(rule_id, file)` — the globally unique index columns — instead of
    /// the non-globally-unique `id` field (PK is `(review_id, id)`, so two
    /// different reviews can share the same `id` value).  Filtering by `id`
    /// alone could stamp multiple unrelated findings with the same task_id.
    ///
    /// The `review_id` filter is intentionally absent: `persist_findings` may
    /// reassign the finding to a newer `review_id` between
    /// `list_spawnable_findings` and this call; the unique `(rule_id, file)`
    /// pair is stable regardless of which review_id the row carries.
    ///
    /// Returns `true` if this caller won the claim, `false` if already claimed.
    pub async fn try_claim_finding(&self, rule_id: &str, file: &str) -> anyhow::Result<bool> {
        // Use Rust-computed RFC3339 timestamp so claimed_at format matches the
        // RFC3339 cutoff used in recover_stale_pending_claims.  SQL
        // CURRENT_TIMESTAMP produces "YYYY-MM-DD HH:MM:SS" (space separator)
        // which sorts lexicographically before "YYYY-MM-DDTHH:MM:SSZ" (T
        // separator), causing every same-day claim to appear stale immediately.
        let now = Utc::now().to_rfc3339();
        let sql = rewrite_placeholders(
            "UPDATE review_findings SET task_id = 'pending', claimed_at = ? \
             WHERE rule_id = ? AND file = ? AND status = 'open' AND task_id IS NULL",
            self.dialect,
        );
        let result = sqlx::query(&sql)
            .bind(&now)
            .bind(rule_id)
            .bind(file)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() == 1)
    }

    /// Reset findings stuck in `task_id='pending'` back to `task_id=NULL` so the
    /// next scheduler cycle can retry spawning.
    ///
    /// Two recovery strategies:
    ///
    /// 1. **Known-task rows** (`real_task_id IS NOT NULL`): both `confirm_task_spawned`
    ///    attempts failed but `enqueue_task` succeeded.  We stored the real task id in
    ///    `real_task_id` so recovery can call `is_task_done(real_task_id)` to verify
    ///    the underlying task has reached a terminal state before resetting the claim.
    ///    This is correct regardless of how many rounds the task ran or how long it
    ///    spent queued — no fixed time threshold is needed.
    ///
    /// 2. **Unknown-task rows** (`real_task_id IS NULL`): the claim is from the narrow
    ///    window between `try_claim_finding` and the `enqueue_task` / `release_claim`
    ///    calls.  A time-based `stale_secs` threshold is appropriate here; this window
    ///    should never exceed a few seconds under normal operation.
    ///
    /// Returns the number of findings recovered.
    pub async fn recover_stale_pending_claims(
        &self,
        stale_secs: i64,
        is_task_done: impl Fn(&str) -> bool,
    ) -> anyhow::Result<u64> {
        // Strategy 1: recover rows whose real underlying task has terminated.
        let select_sql = rewrite_placeholders(
            "SELECT rule_id, file, real_task_id \
             FROM review_findings \
             WHERE task_id = 'pending' AND status = 'open' \
               AND claimed_at IS NOT NULL AND real_task_id IS NOT NULL",
            self.dialect,
        );
        let with_real_id: Vec<(String, String, String)> =
            sqlx::query_as(&select_sql).fetch_all(&self.pool).await?;

        let mut recovered = 0u64;
        let update_real_sql = rewrite_placeholders(
            "UPDATE review_findings \
             SET task_id = NULL, claimed_at = NULL, real_task_id = NULL \
             WHERE rule_id = ? AND file = ? AND status = 'open' \
               AND task_id = 'pending' AND real_task_id = ?",
            self.dialect,
        );
        for (rule_id, file, real_tid) in with_real_id {
            if is_task_done(&real_tid) {
                let res = sqlx::query(&update_real_sql)
                    .bind(&rule_id)
                    .bind(&file)
                    .bind(&real_tid)
                    .execute(&self.pool)
                    .await?;
                recovered += res.rows_affected();
            }
        }

        // Strategy 2: recover mid-claim rows (no real task id yet) via time threshold.
        let cutoff = Utc::now() - chrono::Duration::seconds(stale_secs);
        let cutoff_str = cutoff.to_rfc3339();
        let cutoff_sql = rewrite_placeholders(
            "UPDATE review_findings SET task_id = NULL, claimed_at = NULL \
             WHERE task_id = 'pending' AND status = 'open' \
               AND claimed_at IS NOT NULL AND real_task_id IS NULL \
               AND claimed_at < ?",
            self.dialect,
        );
        let result = sqlx::query(&cutoff_sql)
            .bind(&cutoff_str)
            .execute(&self.pool)
            .await?;
        recovered += result.rows_affected();

        Ok(recovered)
    }

    /// Record the real task id for a finding stuck in `task_id='pending'` after
    /// both `confirm_task_spawned` attempts failed.  This enables task-status-based
    /// stale recovery via [`recover_stale_pending_claims`] without relying on a
    /// fixed time threshold that may not bound actual task lifetime.
    pub async fn record_real_task_id(
        &self,
        rule_id: &str,
        file: &str,
        real_task_id: &str,
    ) -> anyhow::Result<()> {
        let sql = rewrite_placeholders(
            "UPDATE review_findings SET real_task_id = ? \
             WHERE rule_id = ? AND file = ? AND status = 'open' AND task_id = 'pending'",
            self.dialect,
        );
        sqlx::query(&sql)
            .bind(real_task_id)
            .bind(rule_id)
            .bind(file)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Confirm a pending claim by replacing the "pending" sentinel with the
    /// real task_id after a successful enqueue.
    pub async fn confirm_task_spawned(
        &self,
        rule_id: &str,
        file: &str,
        task_id: &str,
    ) -> anyhow::Result<()> {
        let sql = rewrite_placeholders(
            "UPDATE review_findings SET task_id = ? \
             WHERE rule_id = ? AND file = ? AND status = 'open' AND task_id = 'pending'",
            self.dialect,
        );
        sqlx::query(&sql)
            .bind(task_id)
            .bind(rule_id)
            .bind(file)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Release a pending claim by resetting task_id to NULL.
    ///
    /// Called when task enqueue fails after a successful `try_claim_finding`,
    /// allowing the next poll cycle to retry spawning for this finding.
    pub async fn release_claim(&self, rule_id: &str, file: &str) -> anyhow::Result<()> {
        let sql = rewrite_placeholders(
            "UPDATE review_findings SET task_id = NULL \
             WHERE rule_id = ? AND file = ? AND status = 'open' AND task_id = 'pending'",
            self.dialect,
        );
        sqlx::query(&sql)
            .bind(rule_id)
            .bind(file)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    fn rows_to_findings(rows: Vec<FindingRow>) -> Vec<ReviewFinding> {
        rows.into_iter()
            .map(
                |(
                    id,
                    rule_id,
                    priority,
                    impact,
                    confidence,
                    effort,
                    file,
                    line,
                    title,
                    description,
                    action,
                    task_id,
                )| {
                    ReviewFinding {
                        id,
                        rule_id,
                        priority,
                        impact,
                        confidence,
                        effort,
                        file,
                        line,
                        title,
                        description,
                        action,
                        task_id,
                    }
                },
            )
            .collect()
    }
}

/// Parse the agent's JSON output into a ReviewOutput.
///
/// The agent may wrap the JSON in markdown code fences; this function
/// strips them before parsing.
pub fn parse_review_output(raw: &str) -> anyhow::Result<ReviewOutput> {
    let trimmed = raw.trim();

    // Strategy 1: Extract JSON between REVIEW_JSON_START / REVIEW_JSON_END markers.
    const START_MARKER: &str = "REVIEW_JSON_START";
    const END_MARKER: &str = "REVIEW_JSON_END";
    if let Some(s) = trimmed.find(START_MARKER) {
        let after_marker = &trimmed[s + START_MARKER.len()..];
        if let Some(e) = after_marker.find(END_MARKER) {
            let json_slice = after_marker[..e].trim();
            return serde_json::from_str(json_slice)
                .map_err(|e| anyhow::anyhow!("failed to parse marked review JSON: {e}"));
        }
    }

    // Strategy 2 (fallback): Extract between first '{' and last '}'.
    let start = trimmed
        .find('{')
        .ok_or_else(|| anyhow::anyhow!("no JSON object found in review output"))?;
    let end = trimmed
        .rfind('}')
        .map(|i| i + 1)
        .ok_or_else(|| anyhow::anyhow!("no closing brace found in review output"))?;
    serde_json::from_str(&trimmed[start..end])
        .map_err(|e| anyhow::anyhow!("failed to parse review JSON: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_clean_json() -> anyhow::Result<()> {
        let json = r#"{
            "findings": [{
                "id": "F001", "rule_id": "RS-03", "priority": "P2",
                "impact": 3, "confidence": 5, "effort": 2,
                "file": "src/lib.rs", "line": 10,
                "title": "unwrap in prod", "description": "panic risk",
                "action": "use ? operator"
            }],
            "summary": { "p0_count": 0, "p1_count": 0, "p2_count": 1, "p3_count": 0, "health_score": 97 }
        }"#;
        let output = parse_review_output(json)?;
        assert_eq!(output.findings.len(), 1);
        assert_eq!(output.findings[0].rule_id, "RS-03");
        assert_eq!(output.summary.health_score, 97);
        Ok(())
    }

    #[test]
    fn parse_markdown_fenced_json() -> anyhow::Result<()> {
        let raw = "```json\n{\"findings\":[], \"summary\":{\"p0_count\":0,\"p1_count\":0,\"p2_count\":0,\"p3_count\":0,\"health_score\":100}}\n```";
        let output = parse_review_output(raw)?;
        assert!(output.findings.is_empty());
        assert_eq!(output.summary.health_score, 100);
        Ok(())
    }

    #[test]
    fn parse_with_text_before_json() -> anyhow::Result<()> {
        let raw = "Here is my analysis:\n```json\n{\"findings\":[], \"summary\":{\"p0_count\":0,\"p1_count\":0,\"p2_count\":0,\"p3_count\":0,\"health_score\":100}}\n```\nDone.";
        let output = parse_review_output(raw)?;
        assert_eq!(output.summary.health_score, 100);
        Ok(())
    }

    #[test]
    fn parse_marked_json() -> anyhow::Result<()> {
        let raw = "Created issues #1 and #2.\n\nREVIEW_JSON_START\n{\"findings\":[], \"summary\":{\"p0_count\":0,\"p1_count\":0,\"p2_count\":0,\"p3_count\":0,\"health_score\":100}}\nREVIEW_JSON_END\n\nDone.";
        let output = parse_review_output(raw)?;
        assert!(output.findings.is_empty());
        assert_eq!(output.summary.health_score, 100);
        Ok(())
    }

    #[test]
    fn parse_invalid_json_returns_error() {
        let raw = "not json at all";
        assert!(parse_review_output(raw).is_err());
    }

    #[tokio::test]
    async fn concurrent_persist_no_duplicates() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = std::sync::Arc::new(ReviewStore::open(&dir.path().join("review.db")).await?);

        let finding = ReviewFinding {
            id: "F001".into(),
            rule_id: "RS-03".into(),
            priority: "P2".into(),
            impact: 3,
            confidence: 5,
            effort: 2,
            file: "src/lib.rs".into(),
            line: 10,
            title: "unwrap in prod".into(),
            description: "panic risk".into(),
            action: "use ?".into(),
            task_id: None,
        };
        let findings = vec![finding];

        let store1 = store.clone();
        let store2 = store.clone();
        let f1 = findings.clone();
        let f2 = findings.clone();

        let (r1, r2) = tokio::join!(
            store1.persist_findings("rev-1", &f1),
            store2.persist_findings("rev-2", &f2),
        );

        // Both calls must succeed without constraint violation errors.
        r1?;
        r2?;

        // Exactly one open finding must exist (dedup enforced atomically).
        let open = store.list_open().await?;
        assert_eq!(open.len(), 1);

        Ok(())
    }

    fn make_finding(id: &str, rule_id: &str, file: &str, priority: &str) -> ReviewFinding {
        ReviewFinding {
            id: id.into(),
            rule_id: rule_id.into(),
            priority: priority.into(),
            impact: 3,
            confidence: 5,
            effort: 2,
            file: file.into(),
            line: 10,
            title: format!("finding {id}"),
            description: "desc".into(),
            action: "fix it".into(),
            task_id: None,
        }
    }

    #[tokio::test]
    async fn test_claim_confirm_roundtrip() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = ReviewStore::open(&dir.path().join("review.db")).await?;
        let findings = vec![make_finding("F001", "RS-03", "src/lib.rs", "P1")];
        store.persist_findings("rev-1", &findings).await?;

        // First claim wins.
        assert!(
            store.try_claim_finding("RS-03", "src/lib.rs").await?,
            "first claim must succeed"
        );
        // Concurrent poller claim must lose (task_id is 'pending').
        assert!(
            !store.try_claim_finding("RS-03", "src/lib.rs").await?,
            "duplicate claim must return false"
        );
        // Confirm with real task_id.
        store
            .confirm_task_spawned("RS-03", "src/lib.rs", "task-abc")
            .await?;

        let spawnable = store
            .list_spawnable_findings("rev-1", &["P1", "P2"])
            .await?;
        assert!(
            spawnable.is_empty(),
            "confirmed finding must not be returned"
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_release_claim_allows_retry() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = ReviewStore::open(&dir.path().join("review.db")).await?;
        let findings = vec![make_finding("F001", "RS-03", "src/lib.rs", "P1")];
        store.persist_findings("rev-1", &findings).await?;

        // Claim, then release (simulates enqueue failure).
        assert!(store.try_claim_finding("RS-03", "src/lib.rs").await?);
        store.release_claim("RS-03", "src/lib.rs").await?;

        // Must be claimable again after release.
        assert!(
            store.try_claim_finding("RS-03", "src/lib.rs").await?,
            "finding must be claimable after release"
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_list_spawnable_findings_filters_priority() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = ReviewStore::open(&dir.path().join("review.db")).await?;
        let findings = vec![
            make_finding("F0", "R0", "a.rs", "P0"),
            make_finding("F1", "R1", "b.rs", "P1"),
            make_finding("F2", "R2", "c.rs", "P2"),
            make_finding("F3", "R3", "d.rs", "P3"),
        ];
        store.persist_findings("rev-1", &findings).await?;

        let spawnable = store
            .list_spawnable_findings("rev-1", &["P1", "P2"])
            .await?;
        let ids: Vec<&str> = spawnable.iter().map(|f| f.id.as_str()).collect();
        assert!(ids.contains(&"F1"), "P1 must be returned");
        assert!(ids.contains(&"F2"), "P2 must be returned");
        assert!(!ids.contains(&"F0"), "P0 must be excluded");
        assert!(!ids.contains(&"F3"), "P3 must be excluded");
        Ok(())
    }

    #[tokio::test]
    async fn test_list_spawnable_findings_skips_already_spawned() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = ReviewStore::open(&dir.path().join("review.db")).await?;
        let findings = vec![
            make_finding("F1", "R1", "a.rs", "P1"),
            make_finding("F2", "R2", "b.rs", "P2"),
        ];
        store.persist_findings("rev-1", &findings).await?;
        store.try_claim_finding("R1", "a.rs").await?;
        store.confirm_task_spawned("R1", "a.rs", "task-111").await?;

        let spawnable = store
            .list_spawnable_findings("rev-1", &["P1", "P2"])
            .await?;
        assert_eq!(spawnable.len(), 1);
        assert_eq!(spawnable[0].id, "F2");
        Ok(())
    }

    #[tokio::test]
    async fn test_list_spawnable_findings_skips_closed() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = ReviewStore::open(&dir.path().join("review.db")).await?;
        let findings = vec![make_finding("F1", "R1", "a.rs", "P1")];
        store.persist_findings("rev-1", &findings).await?;
        sqlx::query("UPDATE review_findings SET status = 'resolved' WHERE id = 'F1'")
            .execute(&store.pool)
            .await?;

        let spawnable = store
            .list_spawnable_findings("rev-1", &["P1", "P2"])
            .await?;
        assert!(
            spawnable.is_empty(),
            "resolved findings must not be returned"
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_dedup_across_reviews_preserves_task_id() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = ReviewStore::open(&dir.path().join("review.db")).await?;
        let finding = make_finding("F1", "RS-03", "src/lib.rs", "P1");

        // First review: persist, claim and confirm task.
        store
            .persist_findings("rev-1", std::slice::from_ref(&finding))
            .await?;
        store.try_claim_finding("RS-03", "src/lib.rs").await?;
        store
            .confirm_task_spawned("RS-03", "src/lib.rs", "task-orig")
            .await?;

        // Second review: same rule_id+file triggers UPDATE (recurring finding),
        // review_id changes to rev-2 but task_id must be preserved.
        let finding2 = make_finding("F1", "RS-03", "src/lib.rs", "P1");
        store.persist_findings("rev-2", &[finding2]).await?;

        // task_id IS NOT NULL so it must not appear in spawnable list.
        let spawnable = store
            .list_spawnable_findings("rev-2", &["P1", "P2"])
            .await?;
        assert!(
            spawnable.is_empty(),
            "recurring finding with task_id must not be re-spawned"
        );
        Ok(())
    }

    /// Regression test for the TOCTOU race: `persist_findings` changes the
    /// finding's `review_id` *after* `list_spawnable_findings` but *before*
    /// `try_claim_finding`.  The old code filtered by `id` (non-globally-unique)
    /// or `review_id` in the UPDATE; the fixed code filters by `(rule_id, file)`
    /// which is stable regardless of which `review_id` the row carries.
    #[tokio::test]
    async fn test_claim_survives_review_id_change() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = ReviewStore::open(&dir.path().join("review.db")).await?;
        let finding = make_finding("F1", "RS-03", "src/lib.rs", "P1");

        // Simulate: list_spawnable_findings returned F1 under rev-1.
        store
            .persist_findings("rev-1", std::slice::from_ref(&finding))
            .await?;

        // Simulate race: persist_findings runs with rev-2 BEFORE try_claim_finding.
        let finding2 = make_finding("F1", "RS-03", "src/lib.rs", "P1");
        store.persist_findings("rev-2", &[finding2]).await?;

        // try_claim_finding must succeed even though the row now has review_id="rev-2".
        let claimed = store.try_claim_finding("RS-03", "src/lib.rs").await?;
        assert!(claimed, "must succeed even after review_id changed");
        store
            .confirm_task_spawned("RS-03", "src/lib.rs", "task-from-rev1-poller")
            .await?;

        // F1 must no longer appear in spawnable list for rev-2.
        let spawnable = store
            .list_spawnable_findings("rev-2", &["P1", "P2"])
            .await?;
        assert!(
            spawnable.is_empty(),
            "finding with task_id must not be re-spawned after review_id change"
        );
        Ok(())
    }

    #[tokio::test]
    async fn store_persist_and_list() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = ReviewStore::open(&dir.path().join("review.db")).await?;

        let findings = vec![ReviewFinding {
            id: "F001".into(),
            rule_id: "RS-03".into(),
            priority: "P2".into(),
            impact: 3,
            confidence: 5,
            effort: 2,
            file: "src/lib.rs".into(),
            line: 10,
            title: "unwrap in prod".into(),
            description: "panic risk".into(),
            action: "use ?".into(),
            task_id: None,
        }];

        let inserted = store.persist_findings("rev-1", &findings).await?;
        assert_eq!(inserted, 1);

        // Same rule_id + file = dedup (recurring).
        let inserted = store.persist_findings("rev-2", &findings).await?;
        assert_eq!(inserted, 0);

        let open = store.list_open().await?;
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].rule_id, "RS-03");

        Ok(())
    }
}
