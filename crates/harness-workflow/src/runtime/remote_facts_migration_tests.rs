use super::{RemoteFactSnapshot, WorkflowRuntimeStore};
use chrono::Utc;
use serde_json::json;

fn isolated_database_url() -> anyhow::Result<Option<String>> {
    let configured = match std::env::var("HARNESS_DATABASE_URL") {
        Ok(configured) => configured,
        Err(std::env::VarError::NotPresent) => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if configured.trim().is_empty() {
        anyhow::bail!("HARNESS_DATABASE_URL is configured but blank");
    }
    Ok(Some(harness_core::db::resolve_test_database_url(Some(
        &configured,
    ))?))
}

async fn insert_raw_snapshot(
    store: &WorkflowRuntimeStore,
    snapshot: &RemoteFactSnapshot,
) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT INTO remote_fact_snapshots (
            id, provider, repo, subject_type, subject_number, subject_url,
            head_sha, state, fact_hash, facts, fetched_at
         ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10::jsonb, $11)",
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
    .bind(serde_json::to_string(&snapshot.facts)?)
    .bind(snapshot.fetched_at)
    .execute(store.pool())
    .await?;
    Ok(())
}

#[tokio::test]
async fn migration_canonicalizes_remote_fact_collisions_and_preserves_winner() -> anyhow::Result<()>
{
    let Some(database_url) = isolated_database_url()? else {
        return Ok(());
    };
    let dir = tempfile::tempdir()?;
    let store_path = dir.path().join("remote-fact-identity-migration");
    let store =
        WorkflowRuntimeStore::open_with_database_url(&store_path, Some(&database_url)).await?;
    let fetched_at = Utc::now();
    let open = RemoteFactSnapshot::new(
        "github",
        "owner/repo",
        "pull_request",
        902,
        "open",
        json!({"state": "open"}),
        fetched_at,
    );
    let merged = RemoteFactSnapshot::new(
        "github",
        "Owner/Repo",
        "pull_request",
        902,
        "merged",
        json!({"state": "merged"}),
        fetched_at,
    );
    insert_raw_snapshot(&store, &open).await?;
    insert_raw_snapshot(&store, &merged).await?;
    sqlx::query("DELETE FROM schema_migrations WHERE version = 31")
        .execute(store.pool())
        .await?;
    drop(store);

    let store =
        WorkflowRuntimeStore::open_with_database_url(&store_path, Some(&database_url)).await?;
    let migrated = store
        .get_remote_fact_snapshot("github", "owner/repo", "pull_request", 902)
        .await?
        .expect("migration should retain one logical remote fact");
    assert_eq!(migrated.id, merged.id);
    assert_eq!(migrated.repo, "owner/repo");
    assert_eq!(migrated.state, "merged");
    let (count,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM remote_fact_snapshots
         WHERE provider = 'github'
           AND LOWER(repo) = 'owner/repo'
           AND subject_type = 'pull_request'
           AND subject_number = 902",
    )
    .fetch_one(store.pool())
    .await?;
    assert_eq!(count, 1);
    Ok(())
}
