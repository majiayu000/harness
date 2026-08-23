use super::*;
use harness_core::db::resolve_test_database_url;

#[test]
fn workflow_identity_canonicalizes_repo_case() {
    let upper = ProjectWorkflowInstance::new("/tmp/p", Some("Owner/Repo".to_string()));
    let lower = ProjectWorkflowInstance::new("/tmp/p", Some("owner/repo".to_string()));

    assert_eq!(upper.id, lower.id);
    assert_eq!(upper.repo.as_deref(), Some("owner/repo"));
    assert_ne!(
        workflow_id("/tmp/p", Some("Owner/Repo")),
        workflow_id("/tmp/p", Some("owner/repo")),
        "the raw helper must remain able to address legacy identities"
    );
}

async fn open_test_store() -> anyhow::Result<Option<ProjectWorkflowStore>> {
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
        ProjectWorkflowStore::open_with_database_url(
            &dir.path().join("project_workflows.db"),
            Some(&database_url),
        )
        .await?,
    ))
}

#[tokio::test]
async fn project_workflow_store_tracks_repo_state_transitions() -> anyhow::Result<()> {
    let Some(store) = open_test_store().await? else {
        return Ok(());
    };
    let project_id = "/tmp/project-c";
    store
        .record_poll_started(project_id, Some("owner/repo"))
        .await?;
    store
        .record_planning_started(project_id, Some("owner/repo"))
        .await?;
    store
        .record_planner_enqueued(project_id, Some("owner/repo"), "planner-1")
        .await?;
    store
        .record_monitoring_started(project_id, Some("owner/repo"))
        .await?;

    let workflow = store
        .get_by_project(project_id, Some("owner/repo"))
        .await?
        .expect("project workflow");
    assert_eq!(workflow.state, ProjectWorkflowState::Monitoring);
    assert_eq!(
        workflow.active_planner_task_id.as_deref(),
        Some("planner-1")
    );
    Ok(())
}

#[tokio::test]
async fn project_workflow_store_scopes_rows_by_repo() -> anyhow::Result<()> {
    let Some(store) = open_test_store().await? else {
        return Ok(());
    };
    let project_id = "/tmp/shared-project";
    store
        .record_poll_started(project_id, Some("owner/repo-a"))
        .await?;
    store
        .record_poll_started(project_id, Some("owner/repo-b"))
        .await?;

    let a = store
        .get_by_project(project_id, Some("owner/repo-a"))
        .await?
        .expect("repo-a workflow");
    let b = store
        .get_by_project(project_id, Some("owner/repo-b"))
        .await?
        .expect("repo-b workflow");

    assert_ne!(a.id, b.id);
    Ok(())
}

#[tokio::test]
async fn project_workflow_store_reuses_legacy_mixed_case_identity() -> anyhow::Result<()> {
    let Some(store) = open_test_store().await? else {
        return Ok(());
    };
    let project_id = "/tmp/legacy-mixed-case-project";
    let mut legacy = store
        .record_poll_started(project_id, Some("Owner/Repo"))
        .await?;
    sqlx::query("DELETE FROM project_workflows WHERE id = $1")
        .bind(&legacy.id)
        .execute(&store.pool)
        .await?;
    legacy.id = format!("{project_id}::repo:Owner/Repo::project");
    legacy.repo = Some("Owner/Repo".to_string());
    store.upsert(&legacy).await?;
    let updated = store.record_idle(project_id, Some("owner/repo")).await?;

    assert_eq!(updated.id, legacy.id);
    assert_eq!(updated.repo.as_deref(), Some("Owner/Repo"));
    assert_eq!(updated.state, ProjectWorkflowState::Idle);
    assert_eq!(store.row_count().await?, 1);
    let loaded = store
        .get_by_project(project_id, Some("owner/repo"))
        .await?
        .ok_or_else(|| anyhow::anyhow!("canonical lookup should find legacy workflow"))?;
    assert_eq!(loaded.id, legacy.id);
    let index: Option<(String,)> = sqlx::query_as(
        "SELECT indexdef FROM pg_indexes
         WHERE schemaname = current_schema()
           AND tablename = 'project_workflows'
           AND indexname = 'idx_project_workflows_repo_ci'",
    )
    .fetch_optional(&store.pool)
    .await?;
    assert!(index.is_some_and(|(definition,)| {
        let definition = definition.to_ascii_lowercase();
        definition.contains("create unique index")
            && definition.contains("lower(((data)::jsonb ->> 'repo'")
    }));
    Ok(())
}

#[tokio::test]
async fn project_workflow_backfill_skips_case_equivalent_identity() -> anyhow::Result<()> {
    let Some(store) = open_test_store().await? else {
        return Ok(());
    };
    let project_id = "/tmp/backfill-mixed-case-project";
    let canonical = store
        .record_poll_started(project_id, Some("owner/repo"))
        .await?;
    let mut legacy = canonical.clone();
    legacy.id = format!("{project_id}::repo:Owner/Repo::project");
    legacy.repo = Some("Owner/Repo".to_string());

    assert!(!store.insert_if_absent(&legacy).await?);
    assert_eq!(store.row_count().await?, 1);
    assert_eq!(
        store
            .get_by_project(project_id, Some("Owner/Repo"))
            .await?
            .map(|workflow| workflow.id),
        Some(canonical.id)
    );
    Ok(())
}

#[tokio::test]
async fn project_workflow_store_serializes_concurrent_repo_case_variants() -> anyhow::Result<()> {
    let Some(store) = open_test_store().await? else {
        return Ok(());
    };
    let project_id = "/tmp/concurrent-mixed-case-project";
    let upper = store.record_poll_started(project_id, Some("Owner/Repo"));
    let lower = store.record_poll_started(project_id, Some("owner/repo"));
    let (upper, lower) = tokio::join!(upper, lower);
    let upper = upper?;
    let lower = lower?;

    assert_eq!(upper.id, lower.id);
    assert_eq!(upper.repo.as_deref(), Some("owner/repo"));
    assert_eq!(lower.repo.as_deref(), Some("owner/repo"));
    assert_eq!(store.row_count().await?, 1);
    Ok(())
}
