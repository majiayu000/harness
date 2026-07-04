use super::store::WorkflowRuntimeStore;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RepoMemoryOutcome {
    Done,
    Failed,
}

impl RepoMemoryOutcome {
    pub const fn db_value(self) -> &'static str {
        match self {
            Self::Done => "done",
            Self::Failed => "failed",
        }
    }

    fn from_db_value(value: &str) -> anyhow::Result<Self> {
        match value {
            "done" => Ok(Self::Done),
            "failed" => Ok(Self::Failed),
            other => anyhow::bail!("unknown repo memory outcome: {other}"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RepoMemoryKind {
    ValidationCommand,
    FailureLesson,
    ReviewerPattern,
    EnvironmentNote,
}

impl RepoMemoryKind {
    pub const fn db_value(self) -> &'static str {
        match self {
            Self::ValidationCommand => "validation_command",
            Self::FailureLesson => "failure_lesson",
            Self::ReviewerPattern => "reviewer_pattern",
            Self::EnvironmentNote => "environment_note",
        }
    }

    fn from_db_value(value: &str) -> anyhow::Result<Self> {
        match value {
            "validation_command" => Ok(Self::ValidationCommand),
            "failure_lesson" => Ok(Self::FailureLesson),
            "reviewer_pattern" => Ok(Self::ReviewerPattern),
            "environment_note" => Ok(Self::EnvironmentNote),
            other => anyhow::bail!("unknown repo memory kind: {other}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RepoMemoryRecord {
    pub id: Uuid,
    pub repo: String,
    pub activity_class: String,
    pub outcome: RepoMemoryOutcome,
    pub kind: RepoMemoryKind,
    pub payload_json: Value,
    pub evidence_ref: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub last_used_at: Option<DateTime<Utc>>,
    pub use_count: i64,
}

impl RepoMemoryRecord {
    pub fn new(
        repo: impl Into<String>,
        activity_class: impl Into<String>,
        outcome: RepoMemoryOutcome,
        kind: RepoMemoryKind,
        payload_json: Value,
    ) -> Self {
        let now = Utc::now();
        Self {
            id: Uuid::new_v4(),
            repo: repo.into(),
            activity_class: activity_class.into(),
            outcome,
            kind,
            payload_json,
            evidence_ref: None,
            created_at: now,
            updated_at: now,
            last_used_at: None,
            use_count: 0,
        }
    }

    pub fn with_evidence_ref(mut self, evidence_ref: impl Into<String>) -> Self {
        self.evidence_ref = Some(evidence_ref.into());
        self
    }
}

type RepoMemoryRecordRow = (
    Uuid,
    String,
    String,
    String,
    String,
    String,
    Option<String>,
    DateTime<Utc>,
    DateTime<Utc>,
    Option<DateTime<Utc>>,
    i64,
);

fn record_from_row(row: RepoMemoryRecordRow) -> anyhow::Result<RepoMemoryRecord> {
    let (
        id,
        repo,
        activity_class,
        outcome,
        kind,
        payload_json,
        evidence_ref,
        created_at,
        updated_at,
        last_used_at,
        use_count,
    ) = row;
    Ok(RepoMemoryRecord {
        id,
        repo,
        activity_class,
        outcome: RepoMemoryOutcome::from_db_value(&outcome)?,
        kind: RepoMemoryKind::from_db_value(&kind)?,
        payload_json: serde_json::from_str(&payload_json)?,
        evidence_ref,
        created_at,
        updated_at,
        last_used_at,
        use_count,
    })
}

fn validate_record(record: &RepoMemoryRecord) -> anyhow::Result<()> {
    if record.repo.trim().is_empty() {
        anyhow::bail!("repo memory repo must not be empty");
    }
    if record.activity_class.trim().is_empty() {
        anyhow::bail!("repo memory activity_class must not be empty");
    }
    if record.use_count < 0 {
        anyhow::bail!("repo memory use_count must not be negative");
    }
    Ok(())
}

impl WorkflowRuntimeStore {
    pub async fn insert_repo_memory_record(
        &self,
        record: &RepoMemoryRecord,
    ) -> anyhow::Result<RepoMemoryRecord> {
        validate_record(record)?;
        let payload_json = serde_json::to_string(&record.payload_json)?;
        let row = sqlx::query_as::<_, RepoMemoryRecordRow>(
            "INSERT INTO workflow_repo_memory (
                id, repo, activity_class, outcome, kind, payload_json, evidence_ref,
                created_at, updated_at, last_used_at, use_count
             )
             VALUES ($1, $2, $3, $4, $5, $6::jsonb, $7, $8, $9, $10, $11)
             RETURNING id, repo, activity_class, outcome, kind, payload_json::text,
                evidence_ref, created_at, updated_at, last_used_at, use_count",
        )
        .bind(record.id)
        .bind(&record.repo)
        .bind(&record.activity_class)
        .bind(record.outcome.db_value())
        .bind(record.kind.db_value())
        .bind(&payload_json)
        .bind(&record.evidence_ref)
        .bind(record.created_at)
        .bind(record.updated_at)
        .bind(record.last_used_at)
        .bind(record.use_count)
        .fetch_one(self.pool())
        .await?;
        record_from_row(row)
    }

    pub async fn get_repo_memory_record(
        &self,
        id: Uuid,
    ) -> anyhow::Result<Option<RepoMemoryRecord>> {
        let row = sqlx::query_as::<_, RepoMemoryRecordRow>(
            "SELECT id, repo, activity_class, outcome, kind, payload_json::text,
                evidence_ref, created_at, updated_at, last_used_at, use_count
             FROM workflow_repo_memory
             WHERE id = $1",
        )
        .bind(id)
        .fetch_optional(self.pool())
        .await?;
        row.map(record_from_row).transpose()
    }

    pub async fn list_repo_memory_records(
        &self,
        repo: &str,
    ) -> anyhow::Result<Vec<RepoMemoryRecord>> {
        if repo.trim().is_empty() {
            return Ok(Vec::new());
        }
        let rows = sqlx::query_as::<_, RepoMemoryRecordRow>(
            "SELECT id, repo, activity_class, outcome, kind, payload_json::text,
                evidence_ref, created_at, updated_at, last_used_at, use_count
             FROM workflow_repo_memory
             WHERE repo = $1
             ORDER BY created_at DESC, id ASC",
        )
        .bind(repo)
        .fetch_all(self.pool())
        .await?;
        rows.into_iter().map(record_from_row).collect()
    }

    pub async fn delete_repo_memory_record(&self, id: Uuid) -> anyhow::Result<bool> {
        let result = sqlx::query("DELETE FROM workflow_repo_memory WHERE id = $1")
            .bind(id)
            .execute(self.pool())
            .await?;
        Ok(result.rows_affected() > 0)
    }

    pub async fn touch_repo_memory_records(
        &self,
        ids: &[Uuid],
        used_at: DateTime<Utc>,
    ) -> anyhow::Result<u64> {
        if ids.is_empty() {
            return Ok(0);
        }
        let result = sqlx::query(
            "UPDATE workflow_repo_memory
             SET last_used_at = $2,
                 use_count = use_count + 1,
                 updated_at = CURRENT_TIMESTAMP
             WHERE id = ANY($1::uuid[])",
        )
        .bind(ids)
        .bind(used_at)
        .execute(self.pool())
        .await?;
        Ok(result.rows_affected())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_core::db::resolve_database_url;
    use serde_json::json;

    async fn open_repo_memory_test_store() -> anyhow::Result<Option<WorkflowRuntimeStore>> {
        let Ok(database_url) = resolve_database_url(None) else {
            return Ok(None);
        };
        let schema = format!("repo_memory_store_{}", Uuid::new_v4().simple());
        let store =
            WorkflowRuntimeStore::open_with_database_url_and_schema(Some(&database_url), &schema)
                .await?;
        Ok(Some(store))
    }

    #[tokio::test]
    async fn repo_memory_store_crud_roundtrip() -> anyhow::Result<()> {
        let Some(store) = open_repo_memory_test_store().await? else {
            return Ok(());
        };

        let first = RepoMemoryRecord::new(
            "owner/repo",
            "implement_issue",
            RepoMemoryOutcome::Done,
            RepoMemoryKind::ValidationCommand,
            json!({
                "command": "cargo test -p harness-workflow repo_memory_store",
                "result": "passed"
            }),
        )
        .with_evidence_ref("workflow:run-1:event-3");
        let second = RepoMemoryRecord::new(
            "owner/repo",
            "implement_issue",
            RepoMemoryOutcome::Failed,
            RepoMemoryKind::FailureLesson,
            json!({
                "failure_class": "missing_database_url",
                "lesson": "Skip Postgres-backed tests when no database URL is configured"
            }),
        );
        let other_repo = RepoMemoryRecord::new(
            "other/repo",
            "implement_issue",
            RepoMemoryOutcome::Done,
            RepoMemoryKind::EnvironmentNote,
            json!({ "note": "uses bun for web tests" }),
        );

        let inserted_first = store.insert_repo_memory_record(&first).await?;
        let inserted_second = store.insert_repo_memory_record(&second).await?;
        store.insert_repo_memory_record(&other_repo).await?;

        let loaded = store
            .get_repo_memory_record(first.id)
            .await?
            .expect("repo memory record should exist");
        assert_eq!(loaded, inserted_first);
        assert_eq!(loaded.outcome, RepoMemoryOutcome::Done);
        assert_eq!(loaded.kind, RepoMemoryKind::ValidationCommand);
        assert_eq!(
            loaded.evidence_ref.as_deref(),
            Some("workflow:run-1:event-3")
        );

        let owner_repo_records = store.list_repo_memory_records("owner/repo").await?;
        assert_eq!(owner_repo_records.len(), 2);
        assert!(owner_repo_records
            .iter()
            .all(|record| record.repo == "owner/repo"));

        let used_at = DateTime::parse_from_rfc3339("2026-01-02T03:04:05Z")?.with_timezone(&Utc);
        let touched = store
            .touch_repo_memory_records(&[inserted_first.id, inserted_second.id], used_at)
            .await?;
        assert_eq!(touched, 2);
        let touched_first = store
            .get_repo_memory_record(inserted_first.id)
            .await?
            .expect("touched record should exist");
        assert_eq!(touched_first.use_count, 1);
        assert_eq!(touched_first.last_used_at, Some(used_at));

        assert!(store.delete_repo_memory_record(inserted_second.id).await?);
        assert!(!store.delete_repo_memory_record(inserted_second.id).await?);
        assert!(store
            .get_repo_memory_record(inserted_second.id)
            .await?
            .is_none());
        assert_eq!(store.list_repo_memory_records("owner/repo").await?.len(), 1);
        Ok(())
    }

    #[tokio::test]
    async fn repo_memory_store_rejects_blank_repo_and_activity() -> anyhow::Result<()> {
        let Some(store) = open_repo_memory_test_store().await? else {
            return Ok(());
        };

        let blank_repo = RepoMemoryRecord::new(
            " ",
            "implement_issue",
            RepoMemoryOutcome::Done,
            RepoMemoryKind::ValidationCommand,
            json!({ "command": "cargo test" }),
        );
        let error = store
            .insert_repo_memory_record(&blank_repo)
            .await
            .expect_err("blank repo should be rejected before SQL");
        assert!(format!("{error:#}").contains("repo memory repo must not be empty"));

        let blank_activity = RepoMemoryRecord::new(
            "owner/repo",
            "",
            RepoMemoryOutcome::Done,
            RepoMemoryKind::ValidationCommand,
            json!({ "command": "cargo test" }),
        );
        let error = store
            .insert_repo_memory_record(&blank_activity)
            .await
            .expect_err("blank activity class should be rejected before SQL");
        assert!(format!("{error:#}").contains("repo memory activity_class must not be empty"));
        Ok(())
    }
}
