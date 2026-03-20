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
                PRIMARY KEY (review_id, id)
            )",
        )
        .execute(&pool)
        .await?;
        Ok(Self { pool })
    }

    /// Persist findings from a review run, deduplicating against existing open findings.
    pub async fn persist_findings(
        &self,
        review_id: &str,
        findings: &[ReviewFinding],
    ) -> anyhow::Result<usize> {
        let mut inserted = 0;
        for f in findings {
            // Check if an open finding with the same rule_id + file already exists.
            let existing: Option<(String,)> = sqlx::query_as(
                "SELECT id FROM review_findings \
                 WHERE rule_id = ? AND file = ? AND status = 'open' \
                 LIMIT 1",
            )
            .bind(&f.rule_id)
            .bind(&f.file)
            .fetch_optional(&self.pool)
            .await?;

            if existing.is_some() {
                // Mark as recurring — update the review_id to latest.
                sqlx::query(
                    "UPDATE review_findings SET review_id = ? \
                     WHERE rule_id = ? AND file = ? AND status = 'open'",
                )
                .bind(review_id)
                .bind(&f.rule_id)
                .bind(&f.file)
                .execute(&self.pool)
                .await?;
            } else {
                sqlx::query(
                    "INSERT INTO review_findings \
                     (id, review_id, rule_id, priority, impact, confidence, effort, \
                      file, line, title, description, action) \
                     VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                )
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
                .execute(&self.pool)
                .await?;
                inserted += 1;
            }
        }
        Ok(inserted)
    }

    /// List all open findings, ordered by priority then impact.
    pub async fn list_open(&self) -> anyhow::Result<Vec<ReviewFinding>> {
        let rows: Vec<FindingRow> = sqlx::query_as(
            "SELECT id, rule_id, priority, impact, confidence, effort, \
                    file, line, title, description, action \
             FROM review_findings WHERE status = 'open' \
             ORDER BY \
               CASE priority WHEN 'P0' THEN 0 WHEN 'P1' THEN 1 \
                             WHEN 'P2' THEN 2 WHEN 'P3' THEN 3 ELSE 4 END, \
               impact DESC",
        )
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .into_iter()
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
                    }
                },
            )
            .collect())
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
