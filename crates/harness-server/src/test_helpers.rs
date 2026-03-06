use crate::{http::AppState, server::HarnessServer, thread_manager::ThreadManager};
use harness_agents::AgentRegistry;
use harness_core::HarnessConfig;
use std::sync::{atomic::{AtomicBool, AtomicU64}, Arc};
use tokio::sync::{broadcast, RwLock};

pub async fn make_test_state(dir: &std::path::Path) -> anyhow::Result<AppState> {
    make_test_state_with_registry(dir, AgentRegistry::new("test")).await
}

pub async fn make_test_state_with_registry(
    dir: &std::path::Path,
    agent_registry: AgentRegistry,
) -> anyhow::Result<AppState> {
    let server = Arc::new(HarnessServer::new(
        HarnessConfig::default(),
        ThreadManager::new(),
        agent_registry,
    ));
    let tasks = crate::task_runner::TaskStore::open(&dir.join("tasks.db")).await?;
    let events = Arc::new(harness_observe::EventStore::new(dir)?);
    let signal_detector = harness_gc::SignalDetector::new(
        server.config.gc.signal_thresholds.clone().into(),
        harness_core::ProjectId::new(),
    );
    let draft_store = harness_gc::DraftStore::new(dir)?;
    let gc_agent = Arc::new(harness_gc::GcAgent::new(
        harness_gc::gc_agent::GcConfig::default(),
        signal_detector,
        draft_store,
    ));
    let thread_db = crate::thread_db::ThreadDb::open(&dir.join("threads.db")).await?;
    let (notification_tx, _) = broadcast::channel(64);
    Ok(AppState {
        server,
        project_root: dir.to_path_buf(),
        tasks,
        skills: Arc::new(RwLock::new(harness_skills::SkillStore::new())),
        rules: Arc::new(RwLock::new(harness_rules::engine::RuleEngine::new())),
        events,
        gc_agent,
        plans: Arc::new(RwLock::new(std::collections::HashMap::new())),
        thread_db: Some(thread_db),
        plan_db: None,
        interceptors: vec![],
        notification_tx,
        notification_lagged_total: Arc::new(AtomicU64::new(0)),
        notification_lag_log_every: 1,
        notify_tx: None,
        initialized: Arc::new(AtomicBool::new(true)),
    })
}
