use super::WorkflowRuntimeStore;
use crate::runtime::{WorkflowInstance, WorkflowSubject};
use serde_json::json;

fn runtime_identity_test_database_url() -> anyhow::Result<Option<String>> {
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

fn runtime_identity_instance(id: &str, repo: &str) -> WorkflowInstance {
    WorkflowInstance::new(
        "github_issue_pr",
        1,
        "discovered",
        WorkflowSubject::new("issue", "issue:903"),
    )
    .with_id(id)
    .with_server_data(json!({
        "project_id": "/tmp/runtime-identity-migration",
        "repo": repo,
        "issue_number": 903,
    }))
}

fn runtime_pr_identity_instance(id: &str, repo: &str) -> WorkflowInstance {
    WorkflowInstance::new(
        "github_issue_pr",
        1,
        "discovered",
        WorkflowSubject::new("pull_request", "pr:904"),
    )
    .with_id(id)
    .with_server_data(json!({
        "project_id": "/tmp/runtime-pr-identity-migration",
        "repo": repo,
        "pr_number": 904,
    }))
}

#[tokio::test]
async fn migration_rejects_runtime_repo_case_collision_and_enforces_uniqueness(
) -> anyhow::Result<()> {
    let Some(database_url) = runtime_identity_test_database_url()? else {
        return Ok(());
    };
    let dir = tempfile::tempdir()?;
    let store_path = dir.path().join("runtime-identity-migration");
    let store =
        WorkflowRuntimeStore::open_with_database_url(&store_path, Some(&database_url)).await?;
    let canonical = runtime_identity_instance("runtime-owner-repo-903", "owner/repo");
    let legacy = runtime_identity_instance("runtime-Owner-Repo-903", "Owner/Repo");
    store.upsert_instance(&canonical).await?;
    sqlx::query("DROP INDEX idx_workflow_instances_project_repo_issue_ci")
        .execute(store.pool())
        .await?;
    store.upsert_instance(&legacy).await?;
    sqlx::query("DELETE FROM schema_migrations WHERE version = 32")
        .execute(store.pool())
        .await?;
    drop(store);

    let error = match WorkflowRuntimeStore::open_with_database_url(&store_path, Some(&database_url))
        .await
    {
        Ok(_) => anyhow::bail!("migration must not discard active case-colliding workflows"),
        Err(error) => error,
    };
    assert!(format!("{error:#}")
        .contains("workflow_instances contains case-colliding GitHub issue identities"));
    let context = harness_core::db::PgStoreContext::from_legacy_path_schema(
        &store_path,
        Some(&database_url),
    )?;
    let pool = context.open_pool().await?;
    let (count,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM workflow_instances WHERE definition_id = 'github_issue_pr'",
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!(count, 2, "failed migration must preserve both workflows");
    sqlx::query("DELETE FROM workflow_instances WHERE id = $1")
        .bind(&canonical.id)
        .execute(&pool)
        .await?;
    drop(pool);

    let store =
        WorkflowRuntimeStore::open_with_database_url(&store_path, Some(&database_url)).await?;
    assert!(store.get_instance(&legacy.id).await?.is_some());
    let duplicate = runtime_identity_instance("runtime-duplicate-repo-903", "owner/repo");
    store
        .upsert_instance(&duplicate)
        .await
        .expect_err("unique index should reject a second repository-case identity");
    Ok(())
}

#[tokio::test]
async fn migration_rejects_pr_only_repo_case_collision_and_enforces_uniqueness(
) -> anyhow::Result<()> {
    let Some(database_url) = runtime_identity_test_database_url()? else {
        return Ok(());
    };
    let dir = tempfile::tempdir()?;
    let store_path = dir.path().join("runtime-pr-identity-migration");
    let store =
        WorkflowRuntimeStore::open_with_database_url(&store_path, Some(&database_url)).await?;
    let canonical = runtime_pr_identity_instance("runtime-pr-owner-repo-904", "owner/repo");
    let legacy = runtime_pr_identity_instance("runtime-pr-Owner-Repo-904", "Owner/Repo");
    store.upsert_instance(&canonical).await?;
    sqlx::query("DROP INDEX idx_workflow_instances_project_repo_pr_only_ci")
        .execute(store.pool())
        .await?;
    store.upsert_instance(&legacy).await?;
    sqlx::query("DELETE FROM schema_migrations WHERE version = 32")
        .execute(store.pool())
        .await?;
    drop(store);

    let error = match WorkflowRuntimeStore::open_with_database_url(&store_path, Some(&database_url))
        .await
    {
        Ok(_) => anyhow::bail!("migration must not discard active case-colliding PR workflows"),
        Err(error) => error,
    };
    assert!(format!("{error:#}")
        .contains("workflow_instances contains case-colliding GitHub PR identities"));
    let context = harness_core::db::PgStoreContext::from_legacy_path_schema(
        &store_path,
        Some(&database_url),
    )?;
    let pool = context.open_pool().await?;
    let (count,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM workflow_instances
         WHERE definition_id = 'github_issue_pr'
           AND data->'data'->>'issue_number' IS NULL
           AND data->'data'->>'pr_number' = '904'",
    )
    .fetch_one(&pool)
    .await?;
    assert_eq!(count, 2, "failed migration must preserve both PR workflows");
    sqlx::query("DELETE FROM workflow_instances WHERE id = $1")
        .bind(&canonical.id)
        .execute(&pool)
        .await?;
    drop(pool);

    let store =
        WorkflowRuntimeStore::open_with_database_url(&store_path, Some(&database_url)).await?;
    assert!(store.get_instance(&legacy.id).await?.is_some());
    let duplicate = runtime_pr_identity_instance("runtime-pr-duplicate-904", "owner/repo");
    store
        .upsert_instance(&duplicate)
        .await
        .expect_err("unique index should reject a second PR repository-case identity");
    Ok(())
}
