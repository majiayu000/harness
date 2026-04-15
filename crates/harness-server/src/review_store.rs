use serde::{Deserialize, Serialize};
use sqlx::sqlite::SqlitePool;
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

/// Persists review findings to SQLite.
pub struct ReviewStore {
    pool: SqlitePool,
}

impl ReviewStore {
    pub async fn open(db_path: &Path) -> anyhow::Result<Self> {
        let url = format!("sqlite:{}?mode=rwc", db_path.display());
        let pool = SqlitePool::connect(&url).await?;
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS review_findings (
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
                created_at  TEXT NOT NULL DEFAULT (datetime('now')),
                task_id     TEXT,
                PRIMARY KEY (review_id, id)
            )",
        )
        .execute(&pool)
        .await?;
        // Migrate existing databases: add task_id / claimed_at columns if absent.
        // PRAGMA table_info returns (cid, name, type, notnull, dflt_value, pk).
        let columns: Vec<(i32, String, String, i32, Option<String>, i32)> =
            sqlx::query_as("PRAGMA table_info(review_findings)")
                .fetch_all(&pool)
                .await?;
        let has_task_id = columns
            .iter()
            .any(|(_, name, _, _, _, _)| name == "task_id");
        if !has_task_id {
            sqlx::query("ALTER TABLE review_findings ADD COLUMN task_id TEXT")
                .execute(&pool)
                .await?;
        }
        let has_claimed_at = columns
            .iter()
            .any(|(_, name, _, _, _, _)| name == "claimed_at");
        if !has_claimed_at {
            sqlx::query("ALTER TABLE review_findings ADD COLUMN claimed_at TEXT")
                .execute(&pool)
                .await?;
            // Backfill pre-upgrade rows that are already stuck in task_id='pending'.
            // ALTER TABLE sets claimed_at = NULL for all existing rows, but the
            // recovery query requires `claimed_at IS NOT NULL`, so without this
            // backfill those rows would remain permanently dead-lettered.
            // Use datetime('now') so they enter the time-based fallback path in
            // recover_stale_pending_claims.  The caller uses a 3900 s threshold
            // (≥ one full turn timeout), which prevents immediate duplicate-task
            // spawning if any pre-upgrade task was still running at upgrade time.
            sqlx::query(
                "UPDATE review_findings \
                 SET claimed_at = datetime('now') \
                 WHERE task_id = 'pending' AND claimed_at IS NULL",
            )
            .execute(&pool)
            .await?;
        }
        // Migrate: add real_task_id column for confirmed-stale recovery (issue #611).
        // Populated when both confirm_task_spawned attempts fail; allows recovery to
        // gate on actual task completion rather than a fixed time threshold.
        let has_real_task_id = columns
            .iter()
            .any(|(_, name, _, _, _, _)| name == "real_task_id");
        if !has_real_task_id {
            sqlx::query("ALTER TABLE review_findings ADD COLUMN real_task_id TEXT")
                .execute(&pool)
                .await?;
        }
        // Migrate: add cooldown columns for exponential backoff on repeated failures
        // (issue #770).  failure_count tracks how many times the auto-fix task failed;
        // cooldown_until is an ISO-8601 datetime before which spawning is suppressed.
        let has_failure_count = columns
            .iter()
            .any(|(_, name, _, _, _, _)| name == "failure_count");
        if !has_failure_count {
            sqlx::query(
                "ALTER TABLE review_findings ADD COLUMN failure_count INTEGER NOT NULL DEFAULT 0",
            )
            .execute(&pool)
            .await?;
        }
        let has_cooldown_until = columns
            .iter()
            .any(|(_, name, _, _, _, _)| name == "cooldown_until");
        if !has_cooldown_until {
            sqlx::query("ALTER TABLE review_findings ADD COLUMN cooldown_until TEXT")
                .execute(&pool)
                .await?;
        }
        // Migrate: add project_root column to enable per-project scoping of
        // reset_cooldowns_for_resolved (issue #771).  Without this column the
        // reset query clears cooldown state for all projects in a multi-project
        // server when any single project finishes a scan.
        // DEFAULT '' keeps existing rows selectable but they won't match any
        // real project root query, effectively orphaning pre-migration cooldown
        // state (acceptable; the next scan repopulates the correct value).
        let has_project_root = columns
            .iter()
            .any(|(_, name, _, _, _, _)| name == "project_root");
        if !has_project_root {
            sqlx::query(
                "ALTER TABLE review_findings \
                 ADD COLUMN project_root TEXT NOT NULL DEFAULT ''",
            )
            .execute(&pool)
            .await?;
        }
        // Partial index speeds up list_spawnable_findings which filters open rows
        // by review_id, task_id IS NULL and cooldown_until on every scheduler tick.
        // Drop-and-recreate ensures the index is updated if the definition changed
        // (CREATE INDEX IF NOT EXISTS is a no-op on an existing index, even with a
        // different column list).
        sqlx::query("DROP INDEX IF EXISTS idx_rf_spawnable")
            .execute(&pool)
            .await?;
        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_rf_spawnable \
             ON review_findings(review_id, task_id, cooldown_until) WHERE status = 'open'",
        )
        .execute(&pool)
        .await?;
        // Remove duplicate (project_root, rule_id, file, status) rows that may exist
        // from before this unique index was introduced; keep the most recent row per group.
        sqlx::query(
            "DELETE FROM review_findings \
             WHERE rowid NOT IN ( \
                 SELECT MAX(rowid) FROM review_findings GROUP BY project_root, rule_id, file, status \
             )",
        )
        .execute(&pool)
        .await?;
        // Migrate: rebuild the unique dedup index to include project_root so that
        // findings from different projects with the same rule_id+file do not collide.
        sqlx::query("DROP INDEX IF EXISTS idx_finding_dedup")
            .execute(&pool)
            .await?;
        // Unique index enables specific-conflict INSERT deduplication without a
        // TOCTOU race between a SELECT check and an INSERT.
        sqlx::query(
            "CREATE UNIQUE INDEX IF NOT EXISTS idx_finding_dedup \
             ON review_findings(project_root, rule_id, file, status)",
        )
        .execute(&pool)
        .await?;
        Ok(Self { pool })
    }

    /// Persist findings from a review run, deduplicating against existing open findings.
    ///
    /// Same-batch PK duplicates (same id within one review) are caught eagerly and
    /// returned as an error rather than silently dropped by INSERT OR IGNORE.
    /// Cross-review dedup (same rule_id+file already open) is handled atomically by
    /// INSERT OR IGNORE + conditional UPDATE, eliminating the TOCTOU race.
    ///
    /// `project_root` is stored on each row to enable per-project scoping of
    /// `reset_cooldowns_for_resolved` in multi-project server deployments.
    pub async fn persist_findings(
        &self,
        project_root: &str,
        review_id: &str,
        findings: &[ReviewFinding],
    ) -> anyhow::Result<usize> {
        // Detect PK duplicates within the batch before touching the DB.
        // INSERT OR IGNORE would silently drop them; surface an explicit error instead.
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
        for f in findings {
            // Migrate legacy rows that have project_root='' to the current project_root
            // before INSERT OR IGNORE so the dedup index (project_root, rule_id, file, status)
            // correctly treats them as the same finding.  Without this, the INSERT would
            // create a second row (different project_root key) and the subsequent UPDATE
            // would try to rewrite both rows to the same unique key, violating the index.
            sqlx::query(
                "UPDATE review_findings SET project_root = ? \
                 WHERE project_root = '' AND rule_id = ? AND file = ? AND status = 'open'",
            )
            .bind(project_root)
            .bind(&f.rule_id)
            .bind(&f.file)
            .execute(&mut *tx)
            .await?;
            let result = sqlx::query(
                "INSERT OR IGNORE INTO review_findings \
                 (id, review_id, project_root, rule_id, priority, impact, confidence, effort, \
                  file, line, title, description, action) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(&f.id)
            .bind(review_id)
            .bind(project_root)
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
                // Existing open finding for the same rule_id+file — mark as recurring
                // and update project_root in case the row was created before migration.
                // The `(project_root = ? OR project_root = '')` condition ensures that
                // pre-migration rows (project_root='') are correctly backfilled while
                // new rows are matched precisely by project scope.
                sqlx::query(
                    "UPDATE review_findings SET review_id = ?, project_root = ? \
                     WHERE (project_root = ? OR project_root = '') \
                       AND rule_id = ? AND file = ? AND status = 'open'",
                )
                .bind(review_id)
                .bind(project_root)
                .bind(project_root)
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
        let rows: Vec<FindingRow> = sqlx::query_as(
            "SELECT id, rule_id, priority, impact, confidence, effort, \
                    file, line, title, description, action, task_id \
             FROM review_findings WHERE status = 'open' \
             ORDER BY \
               CASE priority WHEN 'P0' THEN 0 WHEN 'P1' THEN 1 \
                             WHEN 'P2' THEN 2 WHEN 'P3' THEN 3 ELSE 4 END, \
               impact DESC",
        )
        .fetch_all(&self.pool)
        .await?;

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
        let sql = format!(
            "SELECT id, rule_id, priority, impact, confidence, effort, \
                    file, line, title, description, action, task_id \
             FROM review_findings \
             WHERE review_id = ? AND priority IN ({placeholders}) \
               AND status = 'open' AND task_id IS NULL \
               AND (cooldown_until IS NULL OR cooldown_until <= datetime('now'))"
        );
        let mut query = sqlx::query_as::<_, FindingRow>(&sql).bind(review_id.to_owned());
        for p in priorities {
            query = query.bind(p.to_string());
        }
        let rows = query.fetch_all(&self.pool).await?;
        Ok(Self::rows_to_findings(rows))
    }

    /// Atomically claim a finding for auto-spawn by setting task_id to "pending".
    ///
    /// Uses `(project_root, rule_id, file)` — the unique index columns — instead of
    /// the non-globally-unique `id` field (PK is `(review_id, id)`, so two
    /// different reviews can share the same `id` value).  Filtering by `id`
    /// alone could stamp multiple unrelated findings with the same task_id.
    ///
    /// The `review_id` filter is intentionally absent: `persist_findings` may
    /// reassign the finding to a newer `review_id` between
    /// `list_spawnable_findings` and this call; the unique `(project_root, rule_id, file)`
    /// tuple is stable regardless of which review_id the row carries.
    ///
    /// Returns `true` if this caller won the claim, `false` if already claimed.
    pub async fn try_claim_finding(
        &self,
        project_root: &str,
        rule_id: &str,
        file: &str,
    ) -> anyhow::Result<bool> {
        let result = sqlx::query(
            "UPDATE review_findings SET task_id = 'pending', claimed_at = datetime('now') \
             WHERE project_root = ? AND rule_id = ? AND file = ? AND status = 'open' \
               AND task_id IS NULL",
        )
        .bind(project_root)
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
    ///    `real_task_id` so recovery can call `get_task_outcome(real_task_id)` to
    ///    determine the terminal state before acting on the claim.
    ///    This is correct regardless of how many rounds the task ran or how long it
    ///    spent queued — no fixed time threshold is needed.
    ///
    /// 2. **Unknown-task rows** (`real_task_id IS NULL`): the claim is from the narrow
    ///    window between `try_claim_finding` and the `enqueue_task` / `release_claim`
    ///    calls.  A time-based `stale_secs` threshold is appropriate here; this window
    ///    should never exceed a few seconds under normal operation.
    ///
    /// The `get_task_outcome` closure returns:
    /// - `None`        — task is still in-flight; leave the claim intact.
    /// - `Some(true)`  — task failed or was cancelled; apply exponential backoff.
    /// - `Some(false)` — task completed successfully; release claim without penalty
    ///   so the next review cycle can re-evaluate the finding.
    ///
    /// Returns the number of findings recovered.
    pub async fn recover_stale_pending_claims(
        &self,
        project_root: &str,
        stale_secs: i64,
        get_task_outcome: impl Fn(&str) -> Option<bool>,
    ) -> anyhow::Result<u64> {
        // Strategy 1: recover rows whose real underlying task has terminated.
        let with_real_id: Vec<(String, String, String)> = sqlx::query_as(
            "SELECT rule_id, file, real_task_id \
             FROM review_findings \
             WHERE project_root = ? AND task_id = 'pending' AND status = 'open' \
               AND claimed_at IS NOT NULL AND real_task_id IS NOT NULL",
        )
        .bind(project_root)
        .fetch_all(&self.pool)
        .await?;

        let mut recovered = 0u64;
        for (rule_id, file, real_tid) in with_real_id {
            match get_task_outcome(&real_tid) {
                Some(true) => {
                    // Task failed or was cancelled — apply exponential backoff so the
                    // same violation is not immediately re-attempted on the next tick.
                    recovered += self
                        .mark_finding_failed(project_root, &rule_id, &file)
                        .await?;
                }
                Some(false) => {
                    // Task completed successfully — release the claim without penalty.
                    // The next review cycle will re-evaluate whether the finding is
                    // still present; treating a success as a failure would incorrectly
                    // increment failure_count and suppress respawn for an open finding.
                    recovered += self
                        .release_stale_claim_ok(project_root, &rule_id, &file)
                        .await?;
                }
                None => {
                    // Task still in-flight — leave the pending claim intact.
                }
            }
        }

        // Strategy 2: recover mid-claim rows (no real task id yet) via time threshold.
        let result = sqlx::query(
            "UPDATE review_findings SET task_id = NULL, claimed_at = NULL \
             WHERE project_root = ? AND task_id = 'pending' AND status = 'open' \
               AND claimed_at IS NOT NULL AND real_task_id IS NULL \
               AND claimed_at < datetime('now', '-' || cast(? as text) || ' seconds')",
        )
        .bind(project_root)
        .bind(stale_secs)
        .execute(&self.pool)
        .await?;
        recovered += result.rows_affected();

        Ok(recovered)
    }

    /// Return the `real_task_id` values for all findings currently stuck in
    /// `task_id='pending'` that have a confirmed real task ID.
    ///
    /// Used by callers to pre-resolve task statuses asynchronously (including
    /// DB-fallback lookups) before passing results to `recover_stale_pending_claims`
    /// via a synchronous closure.  The in-memory `TaskStore` cache only holds active
    /// tasks; terminal tasks (Failed, Cancelled) are DB-only after a server restart,
    /// so the caller must use `get_with_db_fallback` on each returned ID.
    pub async fn list_stale_real_task_ids(
        &self,
        project_root: &str,
    ) -> anyhow::Result<Vec<String>> {
        let rows: Vec<(String,)> = sqlx::query_as(
            "SELECT real_task_id FROM review_findings \
             WHERE project_root = ? AND task_id = 'pending' AND status = 'open' \
               AND claimed_at IS NOT NULL AND real_task_id IS NOT NULL",
        )
        .bind(project_root)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(|(id,)| id).collect())
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
        sqlx::query(
            "UPDATE review_findings SET real_task_id = ? \
             WHERE rule_id = ? AND file = ? AND status = 'open' AND task_id = 'pending'",
        )
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
        project_root: &str,
        rule_id: &str,
        file: &str,
        task_id: &str,
    ) -> anyhow::Result<()> {
        sqlx::query(
            "UPDATE review_findings SET task_id = ? \
             WHERE project_root = ? AND rule_id = ? AND file = ? \
               AND status = 'open' AND task_id = 'pending'",
        )
        .bind(task_id)
        .bind(project_root)
        .bind(rule_id)
        .bind(file)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Record a failed auto-fix attempt and apply exponential backoff cooldown.
    ///
    /// Atomically:
    /// - Releases the pending claim (task_id = NULL)
    /// - Increments failure_count
    /// - Sets cooldown_until = now + min(3600 * 2^failure_count, 86400) seconds
    ///   (1 h → 2 h → 4 h → 8 h → 16 h → 24 h cap)
    ///
    /// Uses the pre-increment failure_count to compute the delay so that the
    /// first failure produces a 1-hour cooldown, the second 2 hours, etc.
    ///
    /// The shift amount is capped at 24 before applying `<<` to avoid SQLite
    /// integer overflow: `1 << 64` wraps to 0 (or goes negative) in SQLite's
    /// 64-bit signed integers, which would collapse the cooldown to `now` after
    /// enough failures and restart the every-tick respawn loop.  Capping at 24
    /// gives `1 << 24 = 16_777_216`; multiplied by 3600 this far exceeds the
    /// 86400-second cap, so the `min()` always fires first at failure_count ≥ 5.
    ///
    /// Returns the number of rows updated (1 if found, 0 if not matching).
    pub async fn mark_finding_failed(
        &self,
        project_root: &str,
        rule_id: &str,
        file: &str,
    ) -> anyhow::Result<u64> {
        let result = sqlx::query(
            "UPDATE review_findings \
             SET task_id = NULL, \
                 claimed_at = NULL, \
                 real_task_id = NULL, \
                 failure_count = failure_count + 1, \
                 cooldown_until = datetime('now', '+' || \
                     cast(min(3600 * (1 << min(failure_count, 24)), 86400) as text) || ' seconds') \
             WHERE project_root = ? AND rule_id = ? AND file = ? AND status = 'open' \
               AND task_id IS NOT NULL",
        )
        .bind(project_root)
        .bind(rule_id)
        .bind(file)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }

    /// Clear the backoff counters for a finding so it can be retried immediately.
    ///
    /// Called when the violation is no longer detected in the latest scan,
    /// ensuring that if it reappears it starts with a fresh failure history.
    pub async fn reset_finding_cooldown(
        &self,
        project_root: &str,
        rule_id: &str,
        file: &str,
    ) -> anyhow::Result<()> {
        sqlx::query(
            "UPDATE review_findings \
             SET failure_count = 0, cooldown_until = NULL \
             WHERE project_root = ? AND rule_id = ? AND file = ? AND status = 'open'",
        )
        .bind(project_root)
        .bind(rule_id)
        .bind(file)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Reset cooldown counters for open findings that did NOT appear in the
    /// latest scan (identified by `current_review_id`).
    ///
    /// When `persist_findings` runs for a new scan, it updates `review_id` to
    /// `current_review_id` for every finding that still exists.  Any open
    /// finding whose `review_id` still points to an older scan was not seen in
    /// this cycle, which means the violation was resolved (manually or via a
    /// previously spawned fix task).  Clearing the counters ensures that if
    /// the violation reappears it starts with a fresh failure history instead
    /// of inheriting stale backoff state.
    ///
    /// `project_root` scopes the reset to findings belonging to the same project.
    /// In a multi-project server all projects share one DB; without this filter
    /// a scan completion for project A would incorrectly clear cooldown state for
    /// open findings from project B whose `review_id != current_review_id`.
    ///
    /// Returns the number of rows whose cooldown was cleared.
    pub async fn reset_cooldowns_for_resolved(
        &self,
        project_root: &str,
        current_review_id: &str,
    ) -> anyhow::Result<u64> {
        let result = sqlx::query(
            "UPDATE review_findings \
             SET failure_count = 0, cooldown_until = NULL \
             WHERE status = 'open' AND project_root = ? AND review_id != ? \
               AND (failure_count > 0 OR cooldown_until IS NOT NULL)",
        )
        .bind(project_root)
        .bind(current_review_id)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }

    /// Release a pending claim after the underlying task completed successfully,
    /// clearing task_id, claimed_at, and real_task_id without penalising
    /// failure_count or setting a cooldown.
    ///
    /// Returns the number of rows updated (1 if found, 0 if the finding was
    /// already in a different state when recovery ran).
    async fn release_stale_claim_ok(
        &self,
        project_root: &str,
        rule_id: &str,
        file: &str,
    ) -> anyhow::Result<u64> {
        let result = sqlx::query(
            "UPDATE review_findings \
             SET task_id = NULL, claimed_at = NULL, real_task_id = NULL \
             WHERE project_root = ? AND rule_id = ? AND file = ? \
               AND status = 'open' AND task_id = 'pending'",
        )
        .bind(project_root)
        .bind(rule_id)
        .bind(file)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }

    /// Release a pending claim by resetting task_id to NULL.
    ///
    /// Called when task enqueue fails after a successful `try_claim_finding`,
    /// allowing the next poll cycle to retry spawning for this finding.
    pub async fn release_claim(
        &self,
        project_root: &str,
        rule_id: &str,
        file: &str,
    ) -> anyhow::Result<()> {
        sqlx::query(
            "UPDATE review_findings SET task_id = NULL \
             WHERE project_root = ? AND rule_id = ? AND file = ? \
               AND status = 'open' AND task_id = 'pending'",
        )
        .bind(project_root)
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
            store1.persist_findings("", "rev-1", &f1),
            store2.persist_findings("", "rev-2", &f2),
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
        store.persist_findings("", "rev-1", &findings).await?;

        // First claim wins.
        assert!(
            store.try_claim_finding("", "RS-03", "src/lib.rs").await?,
            "first claim must succeed"
        );
        // Concurrent poller claim must lose (task_id is 'pending').
        assert!(
            !store.try_claim_finding("", "RS-03", "src/lib.rs").await?,
            "duplicate claim must return false"
        );
        // Confirm with real task_id.
        store
            .confirm_task_spawned("", "RS-03", "src/lib.rs", "task-abc")
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
        store.persist_findings("", "rev-1", &findings).await?;

        // Claim, then release (simulates enqueue failure).
        assert!(store.try_claim_finding("", "RS-03", "src/lib.rs").await?);
        store.release_claim("", "RS-03", "src/lib.rs").await?;

        // Must be claimable again after release.
        assert!(
            store.try_claim_finding("", "RS-03", "src/lib.rs").await?,
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
        store.persist_findings("", "rev-1", &findings).await?;

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
        store.persist_findings("", "rev-1", &findings).await?;
        store.try_claim_finding("", "R1", "a.rs").await?;
        store
            .confirm_task_spawned("", "R1", "a.rs", "task-111")
            .await?;

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
        store.persist_findings("", "rev-1", &findings).await?;
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
            .persist_findings("", "rev-1", std::slice::from_ref(&finding))
            .await?;
        store.try_claim_finding("", "RS-03", "src/lib.rs").await?;
        store
            .confirm_task_spawned("", "RS-03", "src/lib.rs", "task-orig")
            .await?;

        // Second review: same rule_id+file triggers UPDATE (recurring finding),
        // review_id changes to rev-2 but task_id must be preserved.
        let finding2 = make_finding("F1", "RS-03", "src/lib.rs", "P1");
        store.persist_findings("", "rev-2", &[finding2]).await?;

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
            .persist_findings("", "rev-1", std::slice::from_ref(&finding))
            .await?;

        // Simulate race: persist_findings runs with rev-2 BEFORE try_claim_finding.
        let finding2 = make_finding("F1", "RS-03", "src/lib.rs", "P1");
        store.persist_findings("", "rev-2", &[finding2]).await?;

        // try_claim_finding must succeed even though the row now has review_id="rev-2".
        let claimed = store.try_claim_finding("", "RS-03", "src/lib.rs").await?;
        assert!(claimed, "must succeed even after review_id changed");
        store
            .confirm_task_spawned("", "RS-03", "src/lib.rs", "task-from-rev1-poller")
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

        let inserted = store.persist_findings("", "rev-1", &findings).await?;
        assert_eq!(inserted, 1);

        // Same rule_id + file = dedup (recurring).
        let inserted = store.persist_findings("", "rev-2", &findings).await?;
        assert_eq!(inserted, 0);

        let open = store.list_open().await?;
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].rule_id, "RS-03");

        Ok(())
    }

    // -------------------------------------------------------------------------
    // Cooldown / backoff tests (issue #770)
    // -------------------------------------------------------------------------

    /// Helper: read (failure_count, cooldown_until) for an open finding.
    async fn get_cooldown(
        store: &ReviewStore,
        project_root: &str,
        rule_id: &str,
        file: &str,
    ) -> (i64, Option<String>) {
        sqlx::query_as::<_, (i64, Option<String>)>(
            "SELECT failure_count, cooldown_until \
             FROM review_findings \
             WHERE project_root = ? AND rule_id = ? AND file = ? AND status = 'open'",
        )
        .bind(project_root)
        .bind(rule_id)
        .bind(file)
        .fetch_one(&store.pool)
        .await
        .unwrap()
    }

    /// Put a finding into pending state so mark_finding_failed can act on it.
    async fn set_pending(store: &ReviewStore, project_root: &str, rule_id: &str, file: &str) {
        sqlx::query(
            "UPDATE review_findings SET task_id = 'pending', claimed_at = datetime('now') \
             WHERE project_root = ? AND rule_id = ? AND file = ? AND status = 'open'",
        )
        .bind(project_root)
        .bind(rule_id)
        .bind(file)
        .execute(&store.pool)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn test_mark_finding_failed_increments_count() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = ReviewStore::open(&dir.path().join("review.db")).await?;
        store
            .persist_findings("", "rev-1", &[make_finding("F1", "R1", "a.rs", "P1")])
            .await?;

        for expected in 1u32..=3 {
            set_pending(&store, "", "R1", "a.rs").await;
            store.mark_finding_failed("", "R1", "a.rs").await?;
            let (count, _) = get_cooldown(&store, "", "R1", "a.rs").await;
            assert_eq!(
                count as u32, expected,
                "failure_count after {} calls",
                expected
            );
        }
        Ok(())
    }

    #[tokio::test]
    async fn test_cooldown_until_is_future() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = ReviewStore::open(&dir.path().join("review.db")).await?;
        store
            .persist_findings("", "rev-1", &[make_finding("F1", "R1", "a.rs", "P1")])
            .await?;

        set_pending(&store, "", "R1", "a.rs").await;
        store.mark_finding_failed("", "R1", "a.rs").await?;

        let (_, cooldown_until) = get_cooldown(&store, "", "R1", "a.rs").await;
        let ts_str = cooldown_until.expect("cooldown_until must be set after failure");
        // Parse the SQLite datetime string (format: "YYYY-MM-DD HH:MM:SS").
        let dt = chrono::NaiveDateTime::parse_from_str(&ts_str, "%Y-%m-%d %H:%M:%S")
            .expect("cooldown_until must be a valid datetime");
        let now = chrono::Utc::now().naive_utc();
        assert!(dt > now, "cooldown_until must be in the future, got {dt}");
        Ok(())
    }

    #[tokio::test]
    async fn test_exponential_backoff_schedule() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = ReviewStore::open(&dir.path().join("review.db")).await?;
        store
            .persist_findings("", "rev-1", &[make_finding("F1", "R1", "a.rs", "P1")])
            .await?;

        // Expected delays (seconds) for failure_count = 0..=5.
        let expected_secs: &[i64] = &[3600, 7200, 14400, 28800, 57600, 86400];
        for &expected in expected_secs {
            set_pending(&store, "", "R1", "a.rs").await;
            let before = chrono::Utc::now().naive_utc();
            store.mark_finding_failed("", "R1", "a.rs").await?;
            let after = chrono::Utc::now().naive_utc();

            let (_, cooldown_until) = get_cooldown(&store, "", "R1", "a.rs").await;
            let ts_str = cooldown_until.unwrap();
            let dt = chrono::NaiveDateTime::parse_from_str(&ts_str, "%Y-%m-%d %H:%M:%S").unwrap();

            let lower = before + chrono::Duration::seconds(expected) - chrono::Duration::seconds(2);
            let upper = after + chrono::Duration::seconds(expected) + chrono::Duration::seconds(2);
            assert!(
                dt >= lower && dt <= upper,
                "cooldown_until {dt} not in expected window [{lower}, {upper}] \
                 for delay {expected}s"
            );
        }
        Ok(())
    }

    #[tokio::test]
    async fn test_list_spawnable_excludes_cooldown() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = ReviewStore::open(&dir.path().join("review.db")).await?;
        store
            .persist_findings("", "rev-1", &[make_finding("F1", "R1", "a.rs", "P1")])
            .await?;

        // Set an active cooldown (far future).
        sqlx::query(
            "UPDATE review_findings \
             SET cooldown_until = datetime('now', '+3600 seconds') \
             WHERE rule_id = 'R1' AND file = 'a.rs' AND status = 'open'",
        )
        .execute(&store.pool)
        .await?;

        let spawnable = store
            .list_spawnable_findings("rev-1", &["P1", "P2"])
            .await?;
        assert!(
            spawnable.is_empty(),
            "finding with active cooldown must not appear in spawnable list"
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_list_spawnable_includes_expired_cooldown() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = ReviewStore::open(&dir.path().join("review.db")).await?;
        store
            .persist_findings("", "rev-1", &[make_finding("F1", "R1", "a.rs", "P1")])
            .await?;

        // Set an expired cooldown (past timestamp).
        sqlx::query(
            "UPDATE review_findings \
             SET cooldown_until = datetime('now', '-1 seconds') \
             WHERE rule_id = 'R1' AND file = 'a.rs' AND status = 'open'",
        )
        .execute(&store.pool)
        .await?;

        let spawnable = store
            .list_spawnable_findings("rev-1", &["P1", "P2"])
            .await?;
        assert_eq!(
            spawnable.len(),
            1,
            "finding with expired cooldown must appear in spawnable list"
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_reset_clears_cooldown() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = ReviewStore::open(&dir.path().join("review.db")).await?;
        store
            .persist_findings("", "rev-1", &[make_finding("F1", "R1", "a.rs", "P1")])
            .await?;

        // Apply a failure to set counters.
        set_pending(&store, "", "R1", "a.rs").await;
        store.mark_finding_failed("", "R1", "a.rs").await?;
        let (count_before, cooldown_before) = get_cooldown(&store, "", "R1", "a.rs").await;
        assert_eq!(count_before, 1);
        assert!(cooldown_before.is_some());

        store.reset_finding_cooldown("", "R1", "a.rs").await?;

        let (count_after, cooldown_after) = get_cooldown(&store, "", "R1", "a.rs").await;
        assert_eq!(count_after, 0, "failure_count must be 0 after reset");
        assert!(
            cooldown_after.is_none(),
            "cooldown_until must be NULL after reset"
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_strategy1_recovery_triggers_backoff() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = ReviewStore::open(&dir.path().join("review.db")).await?;
        store
            .persist_findings("", "rev-1", &[make_finding("F1", "R1", "a.rs", "P1")])
            .await?;

        // Simulate a confirmed-task pending row (real_task_id set).
        sqlx::query(
            "UPDATE review_findings \
             SET task_id = 'pending', claimed_at = datetime('now'), real_task_id = 'task-xyz' \
             WHERE rule_id = 'R1' AND file = 'a.rs' AND status = 'open'",
        )
        .execute(&store.pool)
        .await?;

        // Strategy-1 recovery: task failed → should trigger backoff.
        let recovered = store
            .recover_stale_pending_claims("", 3900, |_tid| Some(true))
            .await?;
        assert_eq!(recovered, 1, "one row must be recovered");

        let (count, cooldown) = get_cooldown(&store, "", "R1", "a.rs").await;
        assert_eq!(
            count, 1,
            "failure_count must be 1 after strategy-1 recovery"
        );
        assert!(
            cooldown.is_some(),
            "cooldown_until must be set after strategy-1 recovery"
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_strategy2_timeout_no_backoff() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = ReviewStore::open(&dir.path().join("review.db")).await?;
        store
            .persist_findings("", "rev-1", &[make_finding("F1", "R1", "a.rs", "P1")])
            .await?;

        // Simulate a stale mid-claim row (no real_task_id, old claimed_at).
        sqlx::query(
            "UPDATE review_findings \
             SET task_id = 'pending', \
                 claimed_at = datetime('now', '-4000 seconds'), \
                 real_task_id = NULL \
             WHERE rule_id = 'R1' AND file = 'a.rs' AND status = 'open'",
        )
        .execute(&store.pool)
        .await?;

        // Strategy-2 recovery via time threshold — must NOT apply backoff.
        // real_task_id is NULL so the closure is never called; type must match.
        let recovered = store
            .recover_stale_pending_claims("", 3900, |_tid| Some(false))
            .await?;
        assert_eq!(recovered, 1, "one row must be recovered via timeout");

        let (count, cooldown) = get_cooldown(&store, "", "R1", "a.rs").await;
        assert_eq!(
            count, 0,
            "failure_count must remain 0 for strategy-2 recovery"
        );
        assert!(
            cooldown.is_none(),
            "cooldown_until must stay NULL for strategy-2 recovery"
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_resolution_clears_cooldown_state() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = ReviewStore::open(&dir.path().join("review.db")).await?;
        store
            .persist_findings("", "rev-1", &[make_finding("F1", "R1", "a.rs", "P1")])
            .await?;

        // Apply some failures so counters are non-zero.
        set_pending(&store, "", "R1", "a.rs").await;
        store.mark_finding_failed("", "R1", "a.rs").await?;
        set_pending(&store, "", "R1", "a.rs").await;
        store.mark_finding_failed("", "R1", "a.rs").await?;

        let (count_before, cooldown_before) = get_cooldown(&store, "", "R1", "a.rs").await;
        assert_eq!(count_before, 2);
        assert!(cooldown_before.is_some());

        // A new scan (rev-2) does not include this finding.  reset_cooldowns_for_resolved
        // should clear the counters since the finding's review_id is still "rev-1".
        let cleared = store.reset_cooldowns_for_resolved("", "rev-2").await?;
        assert_eq!(cleared, 1, "one finding must have its cooldown cleared");

        let (count_after, cooldown_after) = get_cooldown(&store, "", "R1", "a.rs").await;
        assert_eq!(count_after, 0, "failure_count must be 0 after resolution");
        assert!(
            cooldown_after.is_none(),
            "cooldown_until must be NULL after resolution"
        );
        Ok(())
    }
}
