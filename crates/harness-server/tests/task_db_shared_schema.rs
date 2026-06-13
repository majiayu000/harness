use harness_core::db::{pg_open_pool, resolve_database_url, PgStoreContext};
use harness_core::types::TaskId as CoreTaskId;
use harness_server::task_db::{migrate_legacy_task_db_if_needed, TaskDb, TASK_DB_SCHEMA};
use harness_server::task_runner::{TaskKind, TaskPhase, TaskSchedulerState, TaskState, TaskStatus};

fn unique_test_schema(prefix: &str) -> String {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let count = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock before UNIX epoch")
        .as_nanos();
    format!("{prefix}_{nanos}_{count}")
}

fn make_task(id: &str, description: &str) -> TaskState {
    TaskState {
        id: CoreTaskId(id.to_string()),
        task_kind: TaskKind::Prompt,
        status: TaskStatus::Pending,
        failure_kind: None,
        turn: 0,
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
        description: Some(description.to_string()),
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

#[test]
fn shared_schema_context_uses_fixed_task_db_schema() -> anyhow::Result<()> {
    let context =
        TaskDb::shared_schema_context(Some("postgres://user:pass@localhost:5432/harness"))?;
    assert_eq!(context.schema(), TASK_DB_SCHEMA);
    assert!(
        context.ownership().is_none(),
        "shared task_db schema must not register path-derived ownership"
    );
    Ok(())
}

#[test]
fn store_key_for_data_dir_fails_when_path_cannot_be_canonicalized() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let missing_data_dir = dir.path().join("missing-data-dir");

    let error = TaskDb::store_key_for_data_dir(&missing_data_dir)
        .expect_err("missing data_dir should not produce a fallback store key");

    assert!(
        error
            .to_string()
            .contains("failed to canonicalize task data_dir"),
        "unexpected error: {error:#}"
    );
    Ok(())
}

#[tokio::test]
async fn shared_schema_task_db_keeps_data_dirs_isolated() -> anyhow::Result<()> {
    let database_url = match resolve_database_url(None) {
        Ok(url) => url,
        Err(_) => return Ok(()),
    };
    let dir = tempfile::tempdir()?;
    let setup_pool = pg_open_pool(&database_url).await?;
    let shared_schema = unique_test_schema("task_db_scope_test");
    let shared_context = PgStoreContext::from_schema(&shared_schema, Some(&database_url))?;
    let store_a_dir = dir.path().join("store-a");
    let store_b_dir = dir.path().join("store-b");
    std::fs::create_dir_all(&store_a_dir)?;
    std::fs::create_dir_all(&store_b_dir)?;

    let db_a =
        TaskDb::open_shared_with_data_dir(&shared_context, &setup_pool, &store_a_dir).await?;
    let db_b =
        TaskDb::open_shared_with_data_dir(&shared_context, &setup_pool, &store_b_dir).await?;

    assert_eq!(db_a.schema(), shared_schema);
    assert_eq!(db_b.schema(), shared_schema);
    assert_ne!(db_a.store_key(), db_b.store_key());

    db_a.insert(&make_task("same-task-id", "store-a")).await?;
    db_b.insert(&make_task("same-task-id", "store-b")).await?;

    let task_a = db_a
        .get("same-task-id")
        .await?
        .expect("store a task should exist");
    let task_b = db_b
        .get("same-task-id")
        .await?
        .expect("store b task should exist");
    assert_eq!(task_a.description.as_deref(), Some("store-a"));
    assert_eq!(task_b.description.as_deref(), Some("store-b"));
    assert_eq!(db_a.list().await?.len(), 1);
    assert_eq!(db_b.list().await?.len(), 1);

    setup_pool.close().await;
    Ok(())
}

#[tokio::test]
async fn legacy_path_task_db_backfill_is_idempotent_and_one_time() -> anyhow::Result<()> {
    let database_url = match resolve_database_url(None) {
        Ok(url) => url,
        Err(_) => return Ok(()),
    };
    let dir = tempfile::tempdir()?;
    let setup_pool = pg_open_pool(&database_url).await?;
    let legacy_path = dir.path().join("legacy").join("tasks.db");
    let legacy_db = TaskDb::open_with_database_url(&legacy_path, Some(&database_url)).await?;
    let legacy_task = make_task("legacy-task", "legacy");
    legacy_db.insert(&legacy_task).await?;
    legacy_db
        .write_checkpoint(
            "legacy-task",
            Some("triage"),
            Some("plan"),
            None,
            "plan_done",
        )
        .await?;
    legacy_db
        .insert_artifact("legacy-task", 1, "summary", "artifact")
        .await?;
    legacy_db
        .save_task_prompt("legacy-task", 1, "plan", "prompt")
        .await?;

    let shared_schema = unique_test_schema("task_db_backfill_test");
    let target_context = PgStoreContext::from_schema(&shared_schema, Some(&database_url))?;
    let target_data_dir = dir.path().join("target");
    std::fs::create_dir_all(&target_data_dir)?;
    let target_db =
        TaskDb::open_shared_with_data_dir(&target_context, &setup_pool, &target_data_dir).await?;

    let copied =
        migrate_legacy_task_db_if_needed(&legacy_path, Some(&database_url), &target_db).await?;
    assert_eq!(copied, 1);
    assert!(
        target_db.get("legacy-task").await?.is_some(),
        "legacy task should be copied into target store"
    );
    assert!(
        target_db.load_checkpoint("legacy-task").await?.is_some(),
        "checkpoint should be copied with the task"
    );
    assert_eq!(target_db.list_artifacts("legacy-task").await?.len(), 1);
    assert_eq!(target_db.get_task_prompts("legacy-task").await?.len(), 1);

    let copied_again =
        migrate_legacy_task_db_if_needed(&legacy_path, Some(&database_url), &target_db).await?;
    assert_eq!(copied_again, 0);

    legacy_db
        .insert(&make_task("stale-legacy-task", "stale"))
        .await?;
    let copied_after_stale_update =
        migrate_legacy_task_db_if_needed(&legacy_path, Some(&database_url), &target_db).await?;
    assert_eq!(copied_after_stale_update, 0);
    assert!(
        target_db.get("stale-legacy-task").await?.is_none(),
        "backfill marker should prevent stale legacy reimport on later startup"
    );

    setup_pool.close().await;
    Ok(())
}
