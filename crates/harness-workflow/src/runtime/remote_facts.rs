use super::store::WorkflowRuntimeStore;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RemoteFactSnapshot {
    pub id: Uuid,
    pub provider: String,
    pub repo: String,
    pub subject_type: String,
    pub subject_number: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subject_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub head_sha: Option<String>,
    pub state: String,
    pub fact_hash: String,
    pub facts: Value,
    pub fetched_at: DateTime<Utc>,
}

impl RemoteFactSnapshot {
    pub fn new(
        provider: impl Into<String>,
        repo: impl Into<String>,
        subject_type: impl Into<String>,
        subject_number: i64,
        state: impl Into<String>,
        facts: Value,
        fetched_at: DateTime<Utc>,
    ) -> Self {
        let fact_hash = stable_remote_fact_hash(&facts);
        Self {
            id: Uuid::new_v4(),
            provider: provider.into(),
            repo: repo.into(),
            subject_type: subject_type.into(),
            subject_number,
            subject_url: None,
            head_sha: None,
            state: state.into(),
            fact_hash,
            facts,
            fetched_at,
        }
    }

    pub fn with_subject_url(mut self, subject_url: impl Into<String>) -> Self {
        self.subject_url = Some(subject_url.into());
        self
    }

    pub fn with_head_sha(mut self, head_sha: impl Into<String>) -> Self {
        self.head_sha = Some(head_sha.into());
        self
    }
}

pub fn stable_remote_fact_hash(facts: &Value) -> String {
    let canonical = canonical_json(facts);
    let encoded =
        serde_json::to_vec(&canonical).expect("serde_json::Value serialization cannot fail");
    let digest = Sha256::digest(encoded);
    format!("sha256:{digest:x}")
}

pub fn remote_fact_command_dedupe_key(activity: &str, fact_hash: &str) -> String {
    format!("{activity}:{fact_hash}")
}

fn canonical_json(value: &Value) -> Value {
    match value {
        Value::Array(items) => Value::Array(items.iter().map(canonical_json).collect()),
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort_unstable();
            let mut sorted = Map::new();
            for key in keys {
                sorted.insert(key.clone(), canonical_json(&map[key]));
            }
            Value::Object(sorted)
        }
        scalar => scalar.clone(),
    }
}

type RemoteFactSnapshotRow = (
    Uuid,
    String,
    String,
    String,
    i64,
    Option<String>,
    Option<String>,
    String,
    String,
    String,
    DateTime<Utc>,
);

fn snapshot_from_row(row: RemoteFactSnapshotRow) -> anyhow::Result<RemoteFactSnapshot> {
    let (
        id,
        provider,
        repo,
        subject_type,
        subject_number,
        subject_url,
        head_sha,
        state,
        fact_hash,
        facts,
        fetched_at,
    ) = row;
    Ok(RemoteFactSnapshot {
        id,
        provider,
        repo,
        subject_type,
        subject_number,
        subject_url,
        head_sha,
        state,
        fact_hash,
        facts: serde_json::from_str(&facts)?,
        fetched_at,
    })
}

impl WorkflowRuntimeStore {
    pub async fn upsert_remote_fact_snapshot(
        &self,
        snapshot: &RemoteFactSnapshot,
    ) -> anyhow::Result<RemoteFactSnapshot> {
        let facts = serde_json::to_string(&snapshot.facts)?;
        let row = sqlx::query_as::<_, RemoteFactSnapshotRow>(
            "INSERT INTO remote_fact_snapshots (
                id, provider, repo, subject_type, subject_number, subject_url,
                head_sha, state, fact_hash, facts, fetched_at
             )
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10::jsonb, $11)
             ON CONFLICT (provider, repo, subject_type, subject_number)
             DO UPDATE SET
                id = EXCLUDED.id,
                subject_url = EXCLUDED.subject_url,
                head_sha = EXCLUDED.head_sha,
                state = EXCLUDED.state,
                fact_hash = EXCLUDED.fact_hash,
                facts = EXCLUDED.facts,
                fetched_at = EXCLUDED.fetched_at,
                updated_at = CURRENT_TIMESTAMP
             RETURNING id, provider, repo, subject_type, subject_number, subject_url,
                head_sha, state, fact_hash, facts::text, fetched_at",
        )
        .bind(snapshot.id)
        .bind(&snapshot.provider)
        .bind(&snapshot.repo)
        .bind(&snapshot.subject_type)
        .bind(snapshot.subject_number)
        .bind(&snapshot.subject_url)
        .bind(&snapshot.head_sha)
        .bind(&snapshot.state)
        .bind(&snapshot.fact_hash)
        .bind(&facts)
        .bind(snapshot.fetched_at)
        .fetch_one(self.pool())
        .await?;
        snapshot_from_row(row)
    }

    pub async fn get_remote_fact_snapshot(
        &self,
        provider: &str,
        repo: &str,
        subject_type: &str,
        subject_number: i64,
    ) -> anyhow::Result<Option<RemoteFactSnapshot>> {
        let row = sqlx::query_as::<_, RemoteFactSnapshotRow>(
            "SELECT id, provider, repo, subject_type, subject_number, subject_url,
                head_sha, state, fact_hash, facts::text, fetched_at
             FROM remote_fact_snapshots
             WHERE provider = $1
               AND repo = $2
               AND subject_type = $3
               AND subject_number = $4",
        )
        .bind(provider)
        .bind(repo)
        .bind(subject_type)
        .bind(subject_number)
        .fetch_optional(self.pool())
        .await?;
        row.map(snapshot_from_row).transpose()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_core::db::resolve_database_url;
    use serde_json::json;

    #[test]
    fn stable_remote_fact_hash_sorts_object_keys() {
        let left = json!({
            "repo": "owner/repo",
            "labels": ["bug", "harness"],
            "issue": { "number": 7, "state": "open" }
        });
        let right = json!({
            "issue": { "state": "open", "number": 7 },
            "labels": ["bug", "harness"],
            "repo": "owner/repo"
        });

        assert_eq!(
            stable_remote_fact_hash(&left),
            stable_remote_fact_hash(&right)
        );
    }

    #[test]
    fn remote_fact_command_dedupe_key_uses_fact_hash() {
        assert_eq!(
            remote_fact_command_dedupe_key("implement_issue", "sha256:abc"),
            "implement_issue:sha256:abc"
        );
    }

    #[tokio::test]
    async fn runtime_store_upserts_remote_fact_snapshot_by_subject() -> anyhow::Result<()> {
        if resolve_database_url(None).is_err() {
            return Ok(());
        }

        let dir = tempfile::tempdir()?;
        let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
        let first = RemoteFactSnapshot::new(
            "github",
            "owner/repo",
            "issue",
            7,
            "open",
            json!({ "number": 7, "state": "open" }),
            Utc::now(),
        )
        .with_subject_url("https://github.com/owner/repo/issues/7");
        store.upsert_remote_fact_snapshot(&first).await?;

        let second = RemoteFactSnapshot::new(
            "github",
            "owner/repo",
            "issue",
            7,
            "closed",
            json!({ "number": 7, "state": "closed" }),
            Utc::now(),
        )
        .with_subject_url("https://github.com/owner/repo/issues/7");
        let upserted = store.upsert_remote_fact_snapshot(&second).await?;
        let loaded = store
            .get_remote_fact_snapshot("github", "owner/repo", "issue", 7)
            .await?
            .expect("snapshot should exist");

        assert_eq!(loaded.id, second.id);
        assert_eq!(loaded.state, "closed");
        assert_eq!(loaded.fact_hash, second.fact_hash);
        assert_eq!(loaded, upserted);
        Ok(())
    }
}
