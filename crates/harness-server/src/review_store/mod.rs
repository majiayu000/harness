use harness_core::db::{Migration, PgStoreContext};
use serde::{Deserialize, Serialize};
use sqlx::postgres::PgPool;
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
            id               TEXT NOT NULL,
            review_id        TEXT NOT NULL,
            rule_id          TEXT NOT NULL,
            priority         TEXT NOT NULL,
            impact           INTEGER NOT NULL,
            confidence       INTEGER NOT NULL,
            effort           INTEGER NOT NULL,
            file             TEXT NOT NULL,
            line             BIGINT NOT NULL DEFAULT 0,
            title            TEXT NOT NULL,
            description      TEXT NOT NULL,
            action           TEXT NOT NULL,
            status           TEXT NOT NULL DEFAULT 'open',
            created_at       TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
            task_id          TEXT,
            claimed_at       TIMESTAMPTZ,
            real_task_id     TEXT,
            failure_count    BIGINT NOT NULL DEFAULT 0,
            cooldown_until   TIMESTAMPTZ,
            project_root     TEXT NOT NULL DEFAULT '',
            PRIMARY KEY (review_id, id)
        )",
    },
    Migration {
        version: 2,
        description: "create indexes on review_findings",
        sql: "CREATE INDEX IF NOT EXISTS idx_rf_spawnable \
              ON review_findings(review_id, task_id, cooldown_until) WHERE status = 'open'; \
              CREATE UNIQUE INDEX IF NOT EXISTS idx_finding_dedup \
              ON review_findings(project_root, rule_id, file, status)",
    },
];

/// Persists review findings to Postgres.
pub struct ReviewStore {
    pool: PgPool,
}

impl ReviewStore {
    /// Open (or create) the review store, running any pending migrations.
    pub async fn open(path: &Path) -> anyhow::Result<Self> {
        Self::open_with_database_url(path, None).await
    }

    pub async fn open_with_database_url(
        path: &Path,
        configured_database_url: Option<&str>,
    ) -> anyhow::Result<Self> {
        let pool = PgStoreContext::from_path(path, configured_database_url)?
            .open_migrated_pool(REVIEW_MIGRATIONS)
            .await?;
        Ok(Self { pool })
    }

    /// Persist findings from a review run, deduplicating against existing open findings.
    ///
    /// Same-batch PK duplicates are caught eagerly and returned as an error.
    /// Cross-review dedup (same rule_id+file already open) is handled atomically by
    /// INSERT ON CONFLICT DO NOTHING + conditional UPDATE.
    ///
    /// `project_root` is stored on each row to enable per-project scoping of
    /// `reset_cooldowns_for_resolved` in multi-project server deployments.
    pub async fn persist_findings(
        &self,
        project_root: &str,
        review_id: &str,
        findings: &[ReviewFinding],
    ) -> anyhow::Result<usize> {
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
            sqlx::query(
                "UPDATE review_findings SET project_root = $1 \
                 WHERE project_root = '' AND rule_id = $2 AND file = $3 AND status = 'open'",
            )
            .bind(project_root)
            .bind(&f.rule_id)
            .bind(&f.file)
            .execute(&mut *tx)
            .await?;

            let result = sqlx::query(
                "INSERT INTO review_findings \
                 (id, review_id, project_root, rule_id, priority, impact, confidence, effort, \
                  file, line, title, description, action) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13) \
                 ON CONFLICT (project_root, rule_id, file, status) DO NOTHING",
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
                sqlx::query(
                    "UPDATE review_findings SET review_id = $1, project_root = $2 \
                     WHERE (project_root = $3 OR project_root = '') \
                       AND rule_id = $4 AND file = $5 AND status = 'open'",
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
    /// status='open', no existing task_id, and past any active cooldown.
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
            .enumerate()
            .map(|(i, _)| format!("${}", i + 2))
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "SELECT id, rule_id, priority, impact, confidence, effort, \
                    file, line, title, description, action, task_id \
             FROM review_findings \
             WHERE review_id = $1 AND priority IN ({placeholders}) \
               AND status = 'open' AND task_id IS NULL \
               AND (cooldown_until IS NULL OR cooldown_until <= CURRENT_TIMESTAMP)"
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
    /// Returns `true` if this caller won the claim, `false` if already claimed.
    pub async fn try_claim_finding(
        &self,
        project_root: &str,
        rule_id: &str,
        file: &str,
    ) -> anyhow::Result<bool> {
        let result = sqlx::query(
            "UPDATE review_findings SET task_id = 'pending', claimed_at = CURRENT_TIMESTAMP \
             WHERE project_root = $1 AND rule_id = $2 AND file = $3 AND status = 'open' \
               AND task_id IS NULL",
        )
        .bind(project_root)
        .bind(rule_id)
        .bind(file)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() == 1)
    }

    /// Reset findings stuck in `task_id='pending'` back to `task_id=NULL`.
    ///
    /// Two recovery strategies:
    /// 1. Known-task rows (`real_task_id IS NOT NULL`): resolve via `get_task_outcome`.
    /// 2. Unknown-task rows (`real_task_id IS NULL`): time-based `stale_secs` threshold.
    ///
    /// Returns the number of findings recovered.
    pub async fn recover_stale_pending_claims(
        &self,
        project_root: &str,
        stale_secs: i64,
        get_task_outcome: impl Fn(&str) -> Option<bool>,
    ) -> anyhow::Result<u64> {
        let with_real_id: Vec<(String, String, String)> = sqlx::query_as(
            "SELECT rule_id, file, real_task_id \
             FROM review_findings \
             WHERE project_root = $1 AND task_id = 'pending' AND status = 'open' \
               AND claimed_at IS NOT NULL AND real_task_id IS NOT NULL",
        )
        .bind(project_root)
        .fetch_all(&self.pool)
        .await?;

        let mut recovered = 0u64;
        for (rule_id, file, real_tid) in with_real_id {
            match get_task_outcome(&real_tid) {
                Some(true) => {
                    recovered += self
                        .mark_finding_failed(project_root, &rule_id, &file)
                        .await?;
                }
                Some(false) => {
                    recovered += self
                        .release_stale_claim_ok(project_root, &rule_id, &file)
                        .await?;
                }
                None => {}
            }
        }

        let result = sqlx::query(
            "UPDATE review_findings SET task_id = NULL, claimed_at = NULL \
             WHERE project_root = $1 AND task_id = 'pending' AND status = 'open' \
               AND claimed_at IS NOT NULL AND real_task_id IS NULL \
               AND claimed_at < CURRENT_TIMESTAMP - make_interval(secs => $2::float8)",
        )
        .bind(project_root)
        .bind(stale_secs)
        .execute(&self.pool)
        .await?;
        recovered += result.rows_affected();

        Ok(recovered)
    }

    /// Return `real_task_id` values for all stale pending claims with a confirmed task ID.
    pub async fn list_stale_real_task_ids(
        &self,
        project_root: &str,
    ) -> anyhow::Result<Vec<String>> {
        let rows: Vec<(String,)> = sqlx::query_as(
            "SELECT real_task_id FROM review_findings \
             WHERE project_root = $1 AND task_id = 'pending' AND status = 'open' \
               AND claimed_at IS NOT NULL AND real_task_id IS NOT NULL",
        )
        .bind(project_root)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(|(id,)| id).collect())
    }

    /// Record the real task id for a finding stuck in `task_id='pending'`.
    pub async fn record_real_task_id(
        &self,
        project_root: &str,
        rule_id: &str,
        file: &str,
        real_task_id: &str,
    ) -> anyhow::Result<()> {
        sqlx::query(
            "UPDATE review_findings SET real_task_id = $1 \
             WHERE project_root = $2 AND rule_id = $3 AND file = $4 \
               AND status = 'open' AND task_id = 'pending'",
        )
        .bind(real_task_id)
        .bind(project_root)
        .bind(rule_id)
        .bind(file)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Confirm a pending claim by replacing "pending" with the real task_id.
    pub async fn confirm_task_spawned(
        &self,
        project_root: &str,
        rule_id: &str,
        file: &str,
        task_id: &str,
    ) -> anyhow::Result<()> {
        sqlx::query(
            "UPDATE review_findings SET task_id = $1 \
             WHERE project_root = $2 AND rule_id = $3 AND file = $4 \
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
    /// Atomically: releases claim, increments failure_count, sets cooldown_until
    /// to now + min(3600 * 2^failure_count, 86400) seconds.
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
                 cooldown_until = CURRENT_TIMESTAMP + make_interval(secs => \
                     LEAST(3600.0 * POW(2.0, LEAST(failure_count::float8, 24.0)), 86400.0)) \
             WHERE project_root = $1 AND rule_id = $2 AND file = $3 AND status = 'open' \
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
    pub async fn reset_finding_cooldown(
        &self,
        project_root: &str,
        rule_id: &str,
        file: &str,
    ) -> anyhow::Result<()> {
        sqlx::query(
            "UPDATE review_findings \
             SET failure_count = 0, cooldown_until = NULL \
             WHERE project_root = $1 AND rule_id = $2 AND file = $3 AND status = 'open'",
        )
        .bind(project_root)
        .bind(rule_id)
        .bind(file)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Reset cooldown counters for open findings not seen in the latest scan.
    ///
    /// `project_root` scopes the reset to findings belonging to the same project.
    /// Returns the number of rows whose cooldown was cleared.
    pub async fn reset_cooldowns_for_resolved(
        &self,
        project_root: &str,
        current_review_id: &str,
    ) -> anyhow::Result<u64> {
        let result = sqlx::query(
            "UPDATE review_findings \
             SET failure_count = 0, cooldown_until = NULL \
             WHERE status = 'open' AND project_root = $1 AND review_id != $2 \
               AND (failure_count > 0 OR cooldown_until IS NOT NULL)",
        )
        .bind(project_root)
        .bind(current_review_id)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }

    async fn release_stale_claim_ok(
        &self,
        project_root: &str,
        rule_id: &str,
        file: &str,
    ) -> anyhow::Result<u64> {
        let result = sqlx::query(
            "UPDATE review_findings \
             SET task_id = NULL, claimed_at = NULL, real_task_id = NULL \
             WHERE project_root = $1 AND rule_id = $2 AND file = $3 \
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
    /// Called when task enqueue fails after a successful `try_claim_finding`.
    pub async fn release_claim(
        &self,
        project_root: &str,
        rule_id: &str,
        file: &str,
    ) -> anyhow::Result<()> {
        sqlx::query(
            "UPDATE review_findings SET task_id = NULL \
             WHERE project_root = $1 AND rule_id = $2 AND file = $3 \
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
                )| ReviewFinding {
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
                },
            )
            .collect()
    }

    #[cfg(test)]
    pub(crate) fn pool(&self) -> &PgPool {
        &self.pool
    }
}

/// Parse the agent's JSON output into a ReviewOutput.
///
/// The agent may wrap the JSON in markdown code fences; this function
/// strips them before parsing.
pub fn parse_review_output(raw: &str) -> anyhow::Result<ReviewOutput> {
    let trimmed = raw.trim();

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
}

#[cfg(test)]
mod store_tests;
