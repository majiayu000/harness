use super::test_support::*;
use super::*;
use crate::task_runner::{
    mark_terminal_once, TaskState, TaskStatus, TaskStore, TaskTerminalFailure, TaskTerminalOutcome,
};
use std::path::PathBuf;
use std::sync::Arc;

struct ForceReclaimFixture {
    _root: tempfile::TempDir,
    source_repo: PathBuf,
    lease_store: Arc<WorkspaceLeaseStore>,
    manager: WorkspaceManager,
    task_store: Arc<TaskStore>,
    task_id: harness_core::types::TaskId,
    workspace_path: PathBuf,
}

async fn force_reclaim_fixture(name: &str) -> anyhow::Result<ForceReclaimFixture> {
    let root = crate::test_helpers::tempdir_in_home(&format!("harness-{name}-"))?;
    let source_repo = root.path().join("repo");
    std::fs::create_dir_all(&source_repo)?;
    init_git_repo(&source_repo);
    let branch = current_branch(&source_repo);

    let database_url = crate::test_helpers::test_database_url()?;
    let task_store = TaskStore::open_with_database_url(
        &harness_core::config::dirs::default_db_path(root.path(), "tasks"),
        Some(&database_url),
    )
    .await?;
    let lease_store =
        Arc::new(WorkspaceLeaseStore::open(&root.path().join("workspace-leases")).await?);
    let manager = WorkspaceManager::new_with_pool(
        WorkspaceConfig {
            root: root.path().join("workspaces"),
            ..Default::default()
        },
        WorkspacePoolConfig::default(),
        Some(lease_store.clone()),
    )?;
    let task_id = harness_core::types::TaskId(format!("{name}-task"));
    let lease = manager
        .create_workspace(&task_id, &source_repo, "origin", &branch, 1, None, None)
        .await?;

    let mut state = TaskState::new(task_id.clone());
    state.status = TaskStatus::Implementing;
    state.turn = 3;
    state.workspace_path = Some(lease.workspace_path.clone());
    state.workspace_owner = Some(lease.owner_session);
    state.run_generation = lease.run_generation;
    task_store.insert(&state).await;

    Ok(ForceReclaimFixture {
        _root: root,
        source_repo,
        lease_store,
        manager,
        task_store,
        task_id,
        workspace_path: lease.workspace_path,
    })
}

#[tokio::test]
async fn forced_reclaim_terminalizes_owner_then_deletes_workspace() -> anyhow::Result<()> {
    let _lock = crate::test_helpers::HOME_LOCK.lock().await;
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }
    let fixture = force_reclaim_fixture("forced-reclaim").await?;

    let reclaimed = fixture
        .manager
        .force_reclaim_workspace(
            &fixture.task_store,
            &fixture.source_repo,
            &fixture.workspace_path,
        )
        .await?;

    assert!(reclaimed, "forced reclaim should delete the workspace");
    assert!(
        !fixture.workspace_path.exists(),
        "forced reclaim should remove the workspace directory"
    );
    assert!(
        fixture.lease_store.list_leased().await?.is_empty(),
        "forced reclaim should release the persisted lease"
    );

    let state = fixture
        .task_store
        .get(&fixture.task_id)
        .ok_or_else(|| anyhow::anyhow!("task should remain available"))?;
    assert_eq!(state.status, TaskStatus::Failed);
    let failure = serde_json::from_str::<TaskTerminalFailure>(
        state
            .error
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("forced reclaim should record a terminal reason"))?,
    )
    .map_err(|error| {
        anyhow::anyhow!("terminal reason should parse as TaskTerminalFailure: {error}")
    })?;
    assert_eq!(failure.reason, "workspace_reclaimed");
    assert_eq!(failure.last_status, TaskStatus::Implementing);
    Ok(())
}

#[tokio::test]
async fn reclaim_race_leaves_terminal_task_and_deleted_workspace() -> anyhow::Result<()> {
    let _lock = crate::test_helpers::HOME_LOCK.lock().await;
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }
    let fixture = force_reclaim_fixture("reclaim-race").await?;

    let reclaim = fixture.manager.force_reclaim_workspace(
        &fixture.task_store,
        &fixture.source_repo,
        &fixture.workspace_path,
    );
    let completing_store = fixture.task_store.clone();
    let completing_task = fixture.task_id.clone();
    let complete = async move {
        mark_terminal_once(
            &completing_store,
            &completing_task,
            TaskTerminalOutcome::Completed,
        )
        .await
    };

    let (reclaim_result, complete_result) = tokio::join!(reclaim, complete);
    assert!(
        reclaim_result?,
        "reclaim should delete after a terminal transition wins the race"
    );
    complete_result?;

    let state = fixture
        .task_store
        .get(&fixture.task_id)
        .ok_or_else(|| anyhow::anyhow!("task should remain available"))?;
    assert!(
        state.status.is_terminal(),
        "terminalize-vs-reclaim race must not leave a non-terminal task"
    );
    assert!(
        !fixture.workspace_path.exists(),
        "terminalize-vs-reclaim race should not preserve a reclaimed workspace"
    );
    assert!(
        fixture.lease_store.list_leased().await?.is_empty(),
        "terminalize-vs-reclaim race should release the persisted lease"
    );
    Ok(())
}
