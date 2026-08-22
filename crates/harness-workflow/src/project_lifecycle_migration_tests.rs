use super::{workflow_id, ProjectWorkflowInstance, ProjectWorkflowStore};

fn isolated_database_url() -> anyhow::Result<Option<String>> {
    let Ok(configured) = std::env::var("HARNESS_DATABASE_URL") else {
        return Ok(None);
    };
    Ok(Some(harness_core::db::resolve_test_database_url(Some(
        &configured,
    ))?))
}

#[tokio::test]
async fn migration_deduplicates_project_repo_case_and_enforces_uniqueness() -> anyhow::Result<()> {
    let Some(database_url) = isolated_database_url()? else {
        return Ok(());
    };
    let dir = tempfile::tempdir()?;
    let store_path = dir.path().join("project-workflow-identity-migration");
    let store =
        ProjectWorkflowStore::open_with_database_url(&store_path, Some(&database_url)).await?;
    let project_id = "/tmp/project-identity-migration";
    let canonical =
        ProjectWorkflowInstance::new(project_id.to_string(), Some("owner/repo".to_string()));
    store.upsert(&canonical).await?;
    sqlx::query("DROP INDEX idx_project_workflows_repo_ci")
        .execute(&store.pool)
        .await?;
    let mut legacy = canonical.clone();
    legacy.id = workflow_id(project_id, Some("Owner/Repo"));
    legacy.repo = Some("Owner/Repo".to_string());
    store.upsert(&legacy).await?;
    sqlx::query(
        "UPDATE project_workflows
         SET updated_at = CASE WHEN id = $1
             THEN CURRENT_TIMESTAMP
             ELSE CURRENT_TIMESTAMP - INTERVAL '1 minute'
         END",
    )
    .bind(&legacy.id)
    .execute(&store.pool)
    .await?;
    sqlx::query("DELETE FROM schema_migrations WHERE version = 4")
        .execute(&store.pool)
        .await?;
    drop(store);

    let store =
        ProjectWorkflowStore::open_with_database_url(&store_path, Some(&database_url)).await?;
    assert_eq!(store.row_count().await?, 1);
    let migrated = store
        .get_by_project(project_id, Some("owner/repo"))
        .await?
        .expect("migration should retain one logical project workflow");
    assert_eq!(migrated.id, legacy.id);

    let mut duplicate = canonical;
    duplicate.id = "duplicate-project-workflow-identity".to_string();
    store
        .upsert(&duplicate)
        .await
        .expect_err("unique index should reject a second repository-case identity");
    Ok(())
}
