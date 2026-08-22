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
async fn migration_rejects_project_repo_case_collision_and_enforces_uniqueness(
) -> anyhow::Result<()> {
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
    sqlx::query("DELETE FROM schema_migrations WHERE version = 4")
        .execute(&store.pool)
        .await?;
    drop(store);

    let error = match ProjectWorkflowStore::open_with_database_url(&store_path, Some(&database_url))
        .await
    {
        Ok(_) => anyhow::bail!("migration must not discard a case-colliding workflow"),
        Err(error) => error,
    };
    assert!(format!("{error:#}")
        .contains("project_workflows contains case-colliding repository identities"));
    let context = harness_core::db::PgStoreContext::from_legacy_path_schema(
        &store_path,
        Some(&database_url),
    )?;
    let pool = context.open_pool().await?;
    let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM project_workflows")
        .fetch_one(&pool)
        .await?;
    assert_eq!(count, 2, "failed migration must preserve both workflows");
    sqlx::query("DELETE FROM project_workflows WHERE id = $1")
        .bind(&canonical.id)
        .execute(&pool)
        .await?;
    drop(pool);

    let store =
        ProjectWorkflowStore::open_with_database_url(&store_path, Some(&database_url)).await?;
    assert_eq!(store.row_count().await?, 1);
    let migrated = store
        .get_by_project(project_id, Some("owner/repo"))
        .await?
        .expect("resolved migration should retain the selected project workflow");
    assert_eq!(migrated.id, legacy.id);

    let mut duplicate = canonical;
    duplicate.id = "duplicate-project-workflow-identity".to_string();
    store
        .upsert(&duplicate)
        .await
        .expect_err("unique index should reject a second repository-case identity");
    Ok(())
}
