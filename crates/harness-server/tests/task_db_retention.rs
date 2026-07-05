use harness_core::db::{pg_open_pool, resolve_test_database_url, PgStoreContext, TestSchemaGuard};
use harness_core::types::TaskId as CoreTaskId;
use harness_server::task_db::TaskDb;
use harness_server::task_runner::{TaskKind, TaskPhase, TaskSchedulerState, TaskState, TaskStatus};

fn retention_task_state(id: &str, status: TaskStatus) -> TaskState {
    TaskState {
        id: CoreTaskId(id.to_string()),
        task_kind: TaskKind::Prompt,
        status,
        failure_kind: None,
        turn: 1,
        pr_url: None,
        rounds: vec![],
        error: None,
        source: None,
        external_id: None,
        parent_id: None,
        depends_on: vec![],
        subtask_ids: vec![],
        project_root: None,
        workspace_path: None,
        workspace_owner: None,
        run_generation: 0,
        issue: None,
        description: None,
        created_at: None,
        updated_at: None,
        priority: 0,
        phase: TaskPhase::default(),
        triage_output: None,
        plan_output: None,
        repo: None,
        request_settings: None,
        scheduler: TaskSchedulerState::queued(),
        version: 0,
    }
}

async fn insert_task_with_children(
    db: &TaskDb,
    id: &str,
    status: TaskStatus,
    updated_at: chrono::DateTime<chrono::Utc>,
) -> anyhow::Result<()> {
    db.insert(&retention_task_state(id, status)).await?;
    db.write_checkpoint(id, Some("triage"), Some("plan"), None, "plan_done")
        .await?;
    db.insert_artifact(id, 1, "summary", "artifact").await?;
    db.save_task_prompt(id, 1, "implement", "prompt").await?;
    db.overwrite_updated_at_for_test(id, updated_at).await?;
    Ok(())
}

async fn assert_task_with_children_exists(db: &TaskDb, id: &str) -> anyhow::Result<()> {
    assert!(db.get(id).await?.is_some(), "{id} task should exist");
    assert_eq!(
        db.list_artifacts(id).await?.len(),
        1,
        "{id} artifact should exist"
    );
    assert_eq!(
        db.get_task_prompts(id).await?.len(),
        1,
        "{id} prompt should exist"
    );
    assert!(
        db.load_checkpoint(id).await?.is_some(),
        "{id} checkpoint should exist"
    );
    Ok(())
}

async fn assert_task_with_children_pruned(db: &TaskDb, id: &str) -> anyhow::Result<()> {
    assert!(db.get(id).await?.is_none(), "{id} task should be pruned");
    assert!(
        db.list_artifacts(id).await?.is_empty(),
        "{id} artifacts should be pruned"
    );
    assert!(
        db.get_task_prompts(id).await?.is_empty(),
        "{id} prompts should be pruned"
    );
    assert!(
        db.load_checkpoint(id).await?.is_none(),
        "{id} checkpoint should be pruned"
    );
    Ok(())
}

#[tokio::test]
async fn prune_terminal_tasks_before_removes_only_eligible_terminal_batches() -> anyhow::Result<()>
{
    let database_url = match resolve_test_database_url(None) {
        Ok(url) => url,
        Err(_) => return Ok(()),
    };
    let dir = tempfile::tempdir()?;
    let setup_pool = pg_open_pool(&database_url).await?;
    let mut schema = TestSchemaGuard::new(&database_url, "task_db_retention_batch_test")?;
    let context = PgStoreContext::from_schema(schema.schema(), Some(&database_url))?;
    let data_dir = dir.path().join("store");
    std::fs::create_dir_all(&data_dir)?;
    let db = TaskDb::open_shared_with_data_dir(&context, &setup_pool, &data_dir).await?;

    let now = chrono::Utc::now();
    let cutoff = now - chrono::Duration::days(30);
    insert_task_with_children(
        &db,
        "cancelled-old",
        TaskStatus::Cancelled,
        now - chrono::Duration::days(53),
    )
    .await?;
    insert_task_with_children(
        &db,
        "done-old",
        TaskStatus::Done,
        now - chrono::Duration::days(52),
    )
    .await?;
    insert_task_with_children(
        &db,
        "failed-old",
        TaskStatus::Failed,
        now - chrono::Duration::days(51),
    )
    .await?;
    insert_task_with_children(
        &db,
        "pending-old",
        TaskStatus::Pending,
        now - chrono::Duration::days(54),
    )
    .await?;
    insert_task_with_children(
        &db,
        "implementing-old",
        TaskStatus::Implementing,
        now - chrono::Duration::days(55),
    )
    .await?;
    insert_task_with_children(
        &db,
        "done-new",
        TaskStatus::Done,
        now - chrono::Duration::days(2),
    )
    .await?;

    let first = db.prune_terminal_tasks_before(cutoff, 2).await?;
    assert_eq!(first.tasks_deleted, 2);
    assert_eq!(first.artifacts_deleted, 2);
    assert_eq!(first.prompts_deleted, 2);
    assert_eq!(first.checkpoints_deleted, 2);

    assert_task_with_children_pruned(&db, "cancelled-old").await?;
    assert_task_with_children_pruned(&db, "done-old").await?;
    assert_task_with_children_exists(&db, "failed-old").await?;
    assert_task_with_children_exists(&db, "pending-old").await?;
    assert_task_with_children_exists(&db, "implementing-old").await?;
    assert_task_with_children_exists(&db, "done-new").await?;

    let second = db.prune_terminal_tasks_before(cutoff, 10).await?;
    assert_eq!(second.tasks_deleted, 1);
    assert_eq!(second.artifacts_deleted, 1);
    assert_eq!(second.prompts_deleted, 1);
    assert_eq!(second.checkpoints_deleted, 1);
    assert_task_with_children_pruned(&db, "failed-old").await?;
    assert_task_with_children_exists(&db, "pending-old").await?;
    assert_task_with_children_exists(&db, "implementing-old").await?;
    assert_task_with_children_exists(&db, "done-new").await?;

    let third = db.prune_terminal_tasks_before(cutoff, 0).await?;
    assert!(third.is_empty());
    assert_task_with_children_exists(&db, "pending-old").await?;

    schema.cleanup_with_pool(&setup_pool).await?;
    setup_pool.close().await;
    Ok(())
}

#[tokio::test]
async fn prune_terminal_tasks_before_is_scoped_by_store_key() -> anyhow::Result<()> {
    let database_url = match resolve_test_database_url(None) {
        Ok(url) => url,
        Err(_) => return Ok(()),
    };
    let dir = tempfile::tempdir()?;
    let setup_pool = pg_open_pool(&database_url).await?;
    let mut schema = TestSchemaGuard::new(&database_url, "task_db_retention_scope_test")?;
    let context = PgStoreContext::from_schema(schema.schema(), Some(&database_url))?;
    let store_a_dir = dir.path().join("store-a");
    let store_b_dir = dir.path().join("store-b");
    std::fs::create_dir_all(&store_a_dir)?;
    std::fs::create_dir_all(&store_b_dir)?;
    let db_a = TaskDb::open_shared_with_data_dir(&context, &setup_pool, &store_a_dir).await?;
    let db_b = TaskDb::open_shared_with_data_dir(&context, &setup_pool, &store_b_dir).await?;

    let cutoff = chrono::Utc::now() - chrono::Duration::days(30);
    let old = chrono::Utc::now() - chrono::Duration::days(45);
    insert_task_with_children(&db_a, "same-id", TaskStatus::Done, old).await?;
    insert_task_with_children(&db_b, "same-id", TaskStatus::Done, old).await?;

    let summary = db_a.prune_terminal_tasks_before(cutoff, 10).await?;
    assert_eq!(summary.tasks_deleted, 1);
    assert_task_with_children_pruned(&db_a, "same-id").await?;
    assert_task_with_children_exists(&db_b, "same-id").await?;

    schema.cleanup_with_pool(&setup_pool).await?;
    setup_pool.close().await;
    Ok(())
}
