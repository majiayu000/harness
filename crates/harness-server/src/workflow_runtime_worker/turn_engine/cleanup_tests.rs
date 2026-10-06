use super::turn_lifecycle::{run_turn_lifecycle_with_options, TurnLifecycleOptions};
use crate::server::HarnessServer;
use crate::thread_manager::ThreadManager;
use crate::workspace::{ActiveWorkspace, ActiveWorkspaceState, WorkspaceManager};
use harness_agents::registry::AgentRegistry;
use harness_core::agent::{AgentBackend, AgentRequest, StreamItem};
use harness_core::config::{misc::WorkspaceConfig, HarnessConfig};
use harness_core::error::HarnessError;
use harness_core::types::{AgentId, Item, TaskId, TurnStatus};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, Mutex, Notify, Semaphore};

struct GatedCleanupAdapter {
    state: Mutex<()>,
    drain_started: Notify,
    allow_drain: Semaphore,
    drain_calls: AtomicUsize,
    fail_first_drain: bool,
}

#[async_trait::async_trait]
impl AgentBackend for GatedCleanupAdapter {
    fn name(&self) -> &str {
        "codex"
    }

    async fn start_turn(
        &self,
        _request: AgentRequest,
        _sender: mpsc::Sender<StreamItem>,
    ) -> harness_core::error::Result<()> {
        // Initialization holds this lock until start_turn is cancelled. The
        // timeout path must drop it before calling terminate_and_drain.
        let _state = self.state.lock().await;
        std::future::pending().await
    }

    async fn terminate_and_drain(&self) -> harness_core::error::Result<()> {
        let _state = self.state.lock().await;
        let call = self.drain_calls.fetch_add(1, Ordering::AcqRel);
        if self.fail_first_drain && call == 0 {
            return Err(HarnessError::AgentExecution(
                "injected descendant cleanup failure".to_string(),
            ));
        }
        self.drain_started.notify_one();
        self.allow_drain
            .acquire()
            .await
            .expect("test gate remains open")
            .forget();
        Ok(())
    }
}

async fn assert_wall_timeout_keeps_workspace_until_drained(
    fail_first_drain: bool,
) -> anyhow::Result<()> {
    let root = tempfile::tempdir()?;
    let manager = Arc::new(WorkspaceManager::new(WorkspaceConfig {
        root: root.path().join("workspaces"),
        ..Default::default()
    })?);
    let task_id = TaskId::from_str("pending-cleanup");
    manager.active.insert(
        task_id.clone(),
        ActiveWorkspace {
            workspace_path: root.path().to_path_buf(),
            source_repo: root.path().to_path_buf(),
            repo: None,
            runtime_workflow_id: None,
            workspace_key: "cleanup-test".to_string(),
            project_key: "cleanup-test".to_string(),
            slot_index: 0,
            branch: "test".to_string(),
            created_at: std::time::SystemTime::now(),
            owner_session: manager.owner_session.clone(),
            run_generation: 1,
            acquisition_id: "cleanup-acquisition".to_string(),
            state: ActiveWorkspaceState::Ready,
            _pool_permit: None,
            _repository_write_lease: None,
        },
    );
    let execution_guard = manager.claim_workspace_execution(&task_id, "cleanup-acquisition")?;
    let adapter = Arc::new(GatedCleanupAdapter {
        state: Mutex::new(()),
        drain_started: Notify::new(),
        allow_drain: Semaphore::new(0),
        drain_calls: AtomicUsize::new(0),
        fail_first_drain,
    });
    let mut config = HarnessConfig::default();
    config.server.project_root = root.path().to_path_buf();
    let mut registry = AgentRegistry::new("codex");
    registry.register("codex", adapter.clone());
    let server = Arc::new(HarnessServer::new(config, ThreadManager::new(), registry));
    let thread_id = server
        .thread_manager
        .start_thread(root.path().to_path_buf());
    let turn_id = server.thread_manager.start_turn(
        &thread_id,
        "prompt".to_string(),
        AgentId::from_str("codex"),
    )?;
    let (notification_tx, _) = tokio::sync::broadcast::channel(16);
    let run = {
        let server = server.clone();
        let thread_id = thread_id.clone();
        let turn_id = turn_id.clone();
        let adapter = adapter.clone();
        tokio::spawn(async move {
            run_turn_lifecycle_with_options(
                server,
                None,
                notification_tx,
                thread_id,
                turn_id,
                "prompt".to_string(),
                "codex".to_string(),
                TurnLifecycleOptions {
                    timeout_secs: Some(1),
                    stall_timeout_secs: Some(600),
                    selected_backend: Some(adapter),
                    ..Default::default()
                },
            )
            .await;
            // This is the same ownership boundary as the runtime executor:
            // finalization can begin only once the lifecycle has returned.
            execution_guard.complete();
        })
    };

    tokio::time::timeout(Duration::from_secs(5), adapter.drain_started.notified()).await?;
    assert!(
        !run.is_finished(),
        "the closed drain gate must retain execution"
    );
    let turn = server
        .thread_manager
        .get_turn(&thread_id, &turn_id)
        .expect("turn exists while cleanup is pending");
    assert_eq!(turn.status, TurnStatus::Running);
    assert!(
        manager
            .claim_workspace_execution(&task_id, "cleanup-acquisition")
            .is_err(),
        "a replacement writer must not acquire this workspace"
    );
    assert!(matches!(
        manager
            .active
            .get(&task_id)
            .expect("active workspace")
            .state,
        ActiveWorkspaceState::Running(_)
    ));
    if fail_first_drain {
        assert!(turn.items.iter().any(|item| matches!(
            item,
            Item::Error { message, .. } if message.contains("keeping the workspace reserved")
        )));
    }

    adapter.allow_drain.add_permits(1);
    tokio::time::timeout(Duration::from_secs(2), run).await??;
    assert_eq!(
        adapter.drain_calls.load(Ordering::Acquire),
        if fail_first_drain { 2 } else { 1 }
    );
    let turn = server
        .thread_manager
        .get_turn(&thread_id, &turn_id)
        .expect("turn remains available after drain");
    assert_eq!(turn.status, TurnStatus::Failed);
    assert!(turn.items.iter().any(|item| matches!(
        item,
        Item::Error { message, .. } if message.contains("Agent turn timed out after 1s")
    )));
    if fail_first_drain {
        assert!(turn.items.iter().any(|item| matches!(
            item,
            Item::Error { message, .. } if message.contains("cleanup failed")
                && message.contains("injected descendant cleanup failure")
        )));
    }
    Ok(())
}

#[tokio::test]
async fn wall_timeout_drains_initializing_adapter_before_terminal_and_workspace_release(
) -> anyhow::Result<()> {
    assert_wall_timeout_keeps_workspace_until_drained(false).await
}

#[tokio::test]
async fn wall_timeout_retains_workspace_after_failed_drain_until_retry_succeeds(
) -> anyhow::Result<()> {
    assert_wall_timeout_keeps_workspace_until_drained(true).await
}
