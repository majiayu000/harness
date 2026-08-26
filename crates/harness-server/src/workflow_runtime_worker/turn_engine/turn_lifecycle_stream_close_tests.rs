use super::turn_lifecycle::TurnLifecycleOptions;
use super::turn_lifecycle_tests::{run_test_turn, start_test_turn, CountingAgent};
use crate::{server::HarnessServer, thread_manager::ThreadManager};
use harness_agents::registry::AgentRegistry;
use harness_core::agent::{AgentAdapter, AgentRequest};
use harness_core::config::HarnessConfig;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use tokio::sync::mpsc;

struct ClosedStreamPendingAdapter {
    started: Arc<tokio::sync::Notify>,
    terminate_calls: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl AgentAdapter for ClosedStreamPendingAdapter {
    fn name(&self) -> &str {
        "codex"
    }

    async fn start_turn(
        &self,
        _req: AgentRequest,
        tx: mpsc::Sender<harness_core::agent::AgentEvent>,
    ) -> harness_core::error::Result<()> {
        drop(tx);
        self.started.notify_one();
        std::future::pending::<()>().await;
        Ok(())
    }

    async fn terminate_and_drain(&self) -> harness_core::error::Result<()> {
        self.terminate_calls.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }
}

#[tokio::test]
async fn lease_loss_after_stream_close_still_terminates_execution() -> anyhow::Result<()> {
    let root = tempfile::tempdir()?;
    let started = Arc::new(tokio::sync::Notify::new());
    let stream_closed = Arc::new(tokio::sync::Notify::new());
    let terminate_calls = Arc::new(AtomicUsize::new(0));
    let mut config = HarnessConfig::default();
    config.server.project_root = root.path().to_path_buf();
    config.agents.default_agent = "codex".to_string();
    let mut registry = AgentRegistry::new("codex");
    registry.register(
        "codex",
        Arc::new(CountingAgent {
            calls: Arc::new(AtomicUsize::new(0)),
        }),
    );
    let started_for_factory = started.clone();
    let terminate_calls_for_factory = terminate_calls.clone();
    registry
        .register_turn_backend_factory("codex", move || {
            Arc::new(ClosedStreamPendingAdapter {
                started: started_for_factory.clone(),
                terminate_calls: terminate_calls_for_factory.clone(),
            })
        })
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    let server = Arc::new(HarnessServer::new(config, ThreadManager::new(), registry));
    let turn_id = start_test_turn(&server, root.path())?;
    let (lease_lost, receiver) = tokio::sync::watch::channel(false);
    let root_path = root.path().to_path_buf();
    let stream_closed_for_run = stream_closed.clone();
    let run = tokio::spawn(async move {
        run_test_turn(
            server,
            &root_path,
            turn_id,
            TurnLifecycleOptions {
                lease_lost: Some(receiver),
                stream_closed_observed: Some(stream_closed_for_run),
                ..TurnLifecycleOptions::default()
            },
        )
        .await
    });

    tokio::time::timeout(std::time::Duration::from_secs(2), started.notified())
        .await
        .map_err(|_| anyhow::anyhow!("turn adapter did not start"))?;
    tokio::time::timeout(std::time::Duration::from_secs(2), stream_closed.notified())
        .await
        .map_err(|_| anyhow::anyhow!("turn lifecycle did not observe the closed stream"))?;
    lease_lost.send_replace(true);
    tokio::time::timeout(std::time::Duration::from_secs(2), run)
        .await
        .map_err(|_| anyhow::anyhow!("lease loss was ignored after stream closure"))???;
    assert_eq!(terminate_calls.load(Ordering::Acquire), 1);
    Ok(())
}
