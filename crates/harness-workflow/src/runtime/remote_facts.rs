use super::store::WorkflowRuntimeStore;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
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

pub fn stable_pr_snapshot_fact_hash_input(snapshot: &Value) -> Value {
    let mut stable = snapshot.clone();
    if let Some(object) = stable.as_object_mut() {
        object.remove("observed_at");
        if let Some(repo) = object.get("repo").and_then(Value::as_str) {
            object.insert("repo".to_string(), Value::String(repo.to_ascii_lowercase()));
        }
        object.insert(
            "statusCheckRollup".to_string(),
            json!({
                "state": object
                    .get("status_check_rollup_state")
                    .cloned()
                    .unwrap_or(Value::Null),
                "contexts": object
                    .get("status_check_contexts")
                    .cloned()
                    .unwrap_or_else(|| json!([])),
                "contexts_complete": object
                    .get("status_check_contexts_complete")
                    .cloned()
                    .unwrap_or(Value::Null),
            }),
        );
    }
    stable
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

fn canonical_remote_fact_repo(provider: &str, repo: &str) -> String {
    if provider.eq_ignore_ascii_case("github") {
        repo.to_ascii_lowercase()
    } else {
        repo.to_string()
    }
}

impl WorkflowRuntimeStore {
    pub async fn upsert_remote_fact_snapshot(
        &self,
        snapshot: &RemoteFactSnapshot,
    ) -> anyhow::Result<RemoteFactSnapshot> {
        let mut tx = self.pool().begin().await?;
        let upserted = upsert_remote_fact_snapshot_tx(&mut tx, snapshot).await?;
        tx.commit().await?;
        Ok(upserted)
    }
}

pub(in crate::runtime) async fn upsert_remote_fact_snapshot_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    snapshot: &RemoteFactSnapshot,
) -> anyhow::Result<RemoteFactSnapshot> {
    let facts = serde_json::to_string(&snapshot.facts)?;
    let canonical_repo = canonical_remote_fact_repo(&snapshot.provider, &snapshot.repo);
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
             WHERE remote_fact_snapshots.fetched_at < EXCLUDED.fetched_at
                OR (
                    remote_fact_snapshots.fetched_at = EXCLUDED.fetched_at
                    AND (
                        CASE LOWER(remote_fact_snapshots.state)
                            WHEN 'merged' THEN 4
                            WHEN 'closed' THEN 3
                            WHEN 'done' THEN 3
                            WHEN 'cancelled' THEN 2
                            ELSE 1
                        END,
                        remote_fact_snapshots.fact_hash
                    ) < (
                        CASE LOWER(EXCLUDED.state)
                            WHEN 'merged' THEN 4
                            WHEN 'closed' THEN 3
                            WHEN 'done' THEN 3
                            WHEN 'cancelled' THEN 2
                            ELSE 1
                        END,
                        EXCLUDED.fact_hash
                    )
                )
             RETURNING id, provider, repo, subject_type, subject_number, subject_url,
                head_sha, state, fact_hash, facts::text, fetched_at",
    )
    .bind(snapshot.id)
    .bind(&snapshot.provider)
    .bind(&canonical_repo)
    .bind(&snapshot.subject_type)
    .bind(snapshot.subject_number)
    .bind(&snapshot.subject_url)
    .bind(&snapshot.head_sha)
    .bind(&snapshot.state)
    .bind(&snapshot.fact_hash)
    .bind(&facts)
    .bind(snapshot.fetched_at)
    .fetch_optional(&mut **tx)
    .await?;
    if let Some(row) = row {
        return snapshot_from_row(row);
    }
    let row = sqlx::query_as::<_, RemoteFactSnapshotRow>(
        "SELECT id, provider, repo, subject_type, subject_number, subject_url,
                head_sha, state, fact_hash, facts::text, fetched_at
             FROM remote_fact_snapshots
             WHERE provider = $1 AND repo = $2 AND subject_type = $3 AND subject_number = $4",
    )
    .bind(&snapshot.provider)
    .bind(&canonical_repo)
    .bind(&snapshot.subject_type)
    .bind(snapshot.subject_number)
    .fetch_one(&mut **tx)
    .await?;
    snapshot_from_row(row)
}

impl WorkflowRuntimeStore {
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
               AND (repo = $2 OR ($1 = 'github' AND LOWER(repo) = LOWER($2)))
               AND subject_type = $3
               AND subject_number = $4
             ORDER BY fetched_at DESC, updated_at DESC, (repo = $2) DESC
             LIMIT 1",
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
    use harness_core::db::resolve_test_database_url;
    use serde_json::json;

    async fn remote_fact_test_store() -> anyhow::Result<Option<WorkflowRuntimeStore>> {
        let configured = match std::env::var("HARNESS_DATABASE_URL") {
            Ok(configured) => configured,
            Err(std::env::VarError::NotPresent) => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        if configured.trim().is_empty() {
            anyhow::bail!("HARNESS_DATABASE_URL is configured but blank");
        }
        let database_url = resolve_test_database_url(Some(&configured))?;
        let dir = tempfile::tempdir()?;
        Ok(Some(
            WorkflowRuntimeStore::open_with_database_url(
                &dir.path().join("workflow_runtime.db"),
                Some(&database_url),
            )
            .await?,
        ))
    }

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

    #[test]
    fn github_remote_fact_repo_identity_is_case_insensitive() {
        assert_eq!(
            canonical_remote_fact_repo("github", "Owner/Repo"),
            "owner/repo"
        );
        assert_eq!(
            canonical_remote_fact_repo("custom", "Owner/Repo"),
            "Owner/Repo"
        );
    }

    #[test]
    fn pr_snapshot_hash_input_canonicalizes_repo_case() {
        let upper = stable_pr_snapshot_fact_hash_input(&json!({
            "repo": "Owner/Repo",
            "pr_number": 19,
            "state": "OPEN"
        }));
        let lower = stable_pr_snapshot_fact_hash_input(&json!({
            "repo": "owner/repo",
            "pr_number": 19,
            "state": "OPEN"
        }));

        assert_eq!(
            stable_remote_fact_hash(&upper),
            stable_remote_fact_hash(&lower)
        );
        assert_eq!(upper["repo"], "owner/repo");
    }

    #[tokio::test]
    async fn runtime_store_upserts_remote_fact_snapshot_by_subject() -> anyhow::Result<()> {
        let Some(store) = remote_fact_test_store().await? else {
            return Ok(());
        };
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

    #[tokio::test]
    async fn runtime_store_resolves_legacy_github_repo_case() -> anyhow::Result<()> {
        let Some(store) = remote_fact_test_store().await? else {
            return Ok(());
        };
        let snapshot = RemoteFactSnapshot::new(
            "github",
            "Owner/Repo",
            "pull_request",
            11,
            "open",
            json!({ "number": 11, "state": "open" }),
            Utc::now(),
        );
        store.upsert_remote_fact_snapshot(&snapshot).await?;

        let Some(loaded) = store
            .get_remote_fact_snapshot("github", "owner/repo", "pull_request", 11)
            .await?
        else {
            anyhow::bail!("legacy mixed-case GitHub fact should be found");
        };
        assert_eq!(loaded.id, snapshot.id);
        assert_eq!(loaded.repo, "owner/repo");
        Ok(())
    }

    #[tokio::test]
    async fn runtime_store_canonicalizes_repo_before_competing_timestamp_conflict(
    ) -> anyhow::Result<()> {
        let Some(store) = remote_fact_test_store().await? else {
            return Ok(());
        };
        let fetched_at = Utc::now();
        let newer = RemoteFactSnapshot::new(
            "github",
            "Owner/Repo",
            "pull_request",
            19,
            "merged",
            json!({"state": "merged"}),
            fetched_at,
        );
        store.upsert_remote_fact_snapshot(&newer).await?;
        let older = RemoteFactSnapshot::new(
            "github",
            "owner/repo",
            "pull_request",
            19,
            "open",
            json!({"state": "open"}),
            fetched_at,
        );
        let persisted = store.upsert_remote_fact_snapshot(&older).await?;

        assert_eq!(persisted.id, newer.id);
        assert_eq!(persisted.repo, "owner/repo");
        assert_eq!(persisted.state, "merged");
        let (count,): (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM remote_fact_snapshots
             WHERE provider = 'github'
               AND LOWER(repo) = 'owner/repo'
               AND subject_type = 'pull_request'
               AND subject_number = 19",
        )
        .fetch_one(store.pool())
        .await?;
        assert_eq!(count, 1);

        let open_first = RemoteFactSnapshot::new(
            "github",
            "Owner/Repo",
            "pull_request",
            20,
            "open",
            json!({"state": "open"}),
            fetched_at,
        );
        store.upsert_remote_fact_snapshot(&open_first).await?;
        let merged_second = RemoteFactSnapshot::new(
            "github",
            "owner/repo",
            "pull_request",
            20,
            "merged",
            json!({"state": "merged"}),
            fetched_at,
        );
        let persisted = store.upsert_remote_fact_snapshot(&merged_second).await?;
        assert_eq!(persisted.id, merged_second.id);
        assert_eq!(persisted.state, "merged");
        Ok(())
    }

    #[tokio::test]
    async fn runtime_store_does_not_overwrite_newer_remote_fact() -> anyhow::Result<()> {
        let Some(store) = remote_fact_test_store().await? else {
            return Ok(());
        };
        let fetched_at = Utc::now();
        let newer = RemoteFactSnapshot::new(
            "github",
            "owner/repo",
            "pull_request",
            9,
            "merged",
            json!({"state": "merged"}),
            fetched_at,
        );
        store.upsert_remote_fact_snapshot(&newer).await?;
        let older = RemoteFactSnapshot::new(
            "github",
            "owner/repo",
            "pull_request",
            9,
            "open",
            json!({"state": "open"}),
            fetched_at - chrono::Duration::seconds(1),
        );
        let persisted = store.upsert_remote_fact_snapshot(&older).await?;
        assert_eq!(persisted.fact_hash, newer.fact_hash);
        assert_eq!(persisted.state, "merged");
        Ok(())
    }
}

#[cfg(test)]
#[path = "remote_facts_migration_tests.rs"]
mod migration_tests;
