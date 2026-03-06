use crate::handlers;
use crate::http::AppState;
use harness_protocol::{Method, RpcRequest, RpcResponse};

/// Route a JSON-RPC request to the appropriate handler.
pub async fn handle_request(state: &AppState, req: RpcRequest) -> Option<RpcResponse> {
    use std::sync::atomic::Ordering;

    let id = req.id.clone();

    // Handshake gate: only Initialize and Initialized are allowed before handshake.
    match &req.method {
        Method::Initialize | Method::Initialized => {}
        _ if !state.initialized.load(Ordering::Relaxed) => {
            return Some(RpcResponse::error(
                id,
                harness_protocol::INVALID_REQUEST,
                "Server not initialized. Send 'initialize' first.",
            ));
        }
        _ => {}
    }

    match req.method {
        // === Initialization ===
        Method::Initialize => {
            if state.initialized.load(Ordering::Relaxed) {
                return Some(RpcResponse::error(
                    id,
                    harness_protocol::INVALID_REQUEST,
                    "Server already initialized.",
                ));
            }
            Some(handlers::thread::initialize(id).await)
        }
        Method::Initialized => {
            state.initialized.store(true, Ordering::Relaxed);
            handlers::thread::initialized().await;
            if id.is_none() {
                None
            } else {
                Some(RpcResponse::success(id, serde_json::json!({})))
            }
        }

        // === Thread management ===
        Method::ThreadStart { cwd } => Some(handlers::thread::thread_start(state, id, cwd).await),
        Method::ThreadList => Some(handlers::thread::thread_list(state, id).await),
        Method::ThreadDelete { thread_id } => {
            Some(handlers::thread::thread_delete(state, id, thread_id).await)
        }
        Method::ThreadResume { thread_id } => {
            Some(handlers::thread::thread_resume(state, id, thread_id).await)
        }
        Method::ThreadFork {
            thread_id,
            from_turn,
        } => Some(handlers::thread::thread_fork(state, id, thread_id, from_turn).await),
        Method::ThreadCompact { thread_id } => {
            Some(handlers::thread::thread_compact(state, id, thread_id).await)
        }

        // === Turn control ===
        Method::TurnStart { thread_id, input } => {
            Some(handlers::thread::turn_start(state, id, thread_id, input).await)
        }
        Method::TurnCancel { turn_id } => {
            Some(handlers::thread::turn_cancel(state, id, turn_id).await)
        }
        Method::TurnStatus { turn_id } => {
            Some(handlers::thread::turn_status(state, id, turn_id).await)
        }
        Method::TurnSteer {
            turn_id,
            instruction,
        } => Some(handlers::thread::turn_steer(state, id, turn_id, instruction).await),

        // === Skills ===
        Method::SkillCreate { name, content } => {
            Some(handlers::skills::skill_create(state, id, name, content).await)
        }
        Method::SkillList { query } => Some(handlers::skills::skill_list(state, id, query).await),
        Method::SkillGet { skill_id } => {
            Some(handlers::skills::skill_get(state, id, skill_id).await)
        }
        Method::SkillDelete { skill_id } => {
            Some(handlers::skills::skill_delete(state, id, skill_id).await)
        }

        // === Events / Metrics ===
        Method::EventLog { event } => Some(handlers::observe::event_log(state, id, event).await),
        Method::EventQuery { filters } => {
            Some(handlers::observe::event_query(state, id, filters).await)
        }
        Method::MetricsCollect { project_root } => {
            Some(handlers::observe::metrics_collect(state, id, project_root).await)
        }
        Method::MetricsQuery { filters } => {
            Some(handlers::observe::metrics_query(state, id, filters).await)
        }

        // === Rules ===
        Method::RuleLoad { project_root } => {
            Some(handlers::rules::rule_load(state, id, project_root).await)
        }
        Method::RuleCheck {
            project_root,
            files,
        } => Some(handlers::rules::rule_check(state, id, project_root, files).await),

        // === GC ===
        Method::GcRun { project_id: _ } => Some(handlers::gc::gc_run(state, id).await),
        Method::GcStatus => Some(handlers::gc::gc_status(state, id).await),
        Method::GcDrafts { project_id: _ } => Some(handlers::gc::gc_drafts(state, id).await),
        Method::GcAdopt { draft_id } => Some(handlers::gc::gc_adopt(state, id, draft_id).await),
        Method::GcReject { draft_id, reason } => {
            Some(handlers::gc::gc_reject(state, id, draft_id, reason).await)
        }

        // === ExecPlan ===
        Method::ExecPlanInit { spec, project_root } => {
            Some(handlers::exec::exec_plan_init(state, id, spec, project_root).await)
        }
        Method::ExecPlanStatus { plan_id } => {
            Some(handlers::exec::exec_plan_status(state, id, plan_id).await)
        }
        Method::ExecPlanUpdate { plan_id, updates } => {
            Some(handlers::exec::exec_plan_update(state, id, plan_id, updates).await)
        }

        // === Task classification ===
        Method::TaskClassify { prompt, issue, pr } => {
            Some(handlers::classify::task_classify(id, prompt, issue, pr).await)
        }

        // === Learn feedback loop ===
        Method::LearnRules { project_root } => {
            Some(handlers::learn::learn_rules(state, id, project_root).await)
        }
        Method::LearnSkills { project_root } => {
            Some(handlers::learn::learn_skills(state, id, project_root).await)
        }

        // === Health & Stats ===
        Method::HealthCheck { project_root } => {
            Some(handlers::health::health_check(state, id, project_root).await)
        }
        Method::StatsQuery { since, until } => {
            Some(handlers::health::stats_query(state, id, since, until).await)
        }

        // === VibeGuard ===
        Method::Preflight {
            project_root,
            task_description,
        } => Some(handlers::preflight::preflight(state, id, project_root, task_description).await),
        Method::CrossReview {
            project_root,
            target,
            max_rounds,
        } => Some(
            handlers::cross_review::cross_review(state, id, project_root, target, max_rounds).await,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{http::AppState, server::HarnessServer, thread_manager::ThreadManager};
    use harness_agents::AgentRegistry;
    use harness_core::HarnessConfig;
    use harness_protocol::{Method, RpcRequest};
    use std::sync::Arc;
    use tokio::sync::RwLock;

    async fn make_test_state(dir: &std::path::Path) -> anyhow::Result<AppState> {
        make_test_state_with_config_and_registry(
            dir,
            HarnessConfig::default(),
            AgentRegistry::new("test"),
        )
        .await
    }

    async fn make_test_state_with_registry(
        dir: &std::path::Path,
        agent_registry: AgentRegistry,
    ) -> anyhow::Result<AppState> {
        make_test_state_with_config_and_registry(dir, HarnessConfig::default(), agent_registry)
            .await
    }

    async fn make_test_state_with_config_and_registry(
        dir: &std::path::Path,
        config: HarnessConfig,
        agent_registry: AgentRegistry,
    ) -> anyhow::Result<AppState> {
        let server = Arc::new(HarnessServer::new(
            config,
            ThreadManager::new(),
            agent_registry,
        ));
        let tasks = crate::task_runner::TaskStore::open(&dir.join("tasks.db")).await?;
        let events = Arc::new(harness_observe::EventStore::new(dir)?);
        let signal_detector = harness_gc::SignalDetector::new(
            harness_gc::signal_detector::SignalThresholds::default(),
            harness_core::ProjectId::new(),
        );
        let draft_store = harness_gc::DraftStore::new(dir)?;
        let gc_agent = Arc::new(harness_gc::GcAgent::new(
            harness_gc::gc_agent::GcConfig::default(),
            signal_detector,
            draft_store,
        ));
        let thread_db = crate::thread_db::ThreadDb::open(&dir.join("threads.db")).await?;
        let (notification_tx, _) = tokio::sync::broadcast::channel(64);
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
            interceptors: vec![],
            notification_tx,
            notification_lagged_total: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            notification_lag_log_every: 1,
            notify_tx: None,
            initialized: Arc::new(std::sync::atomic::AtomicBool::new(true)),
        })
    }

    fn writable_home() -> std::path::PathBuf {
        let home = std::path::PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into()));
        if tempfile::Builder::new()
            .prefix("harness-home-probe-")
            .tempdir_in(&home)
            .is_ok()
        {
            return home;
        }

        let fallback = std::env::current_dir()
            .expect("resolve cwd")
            .join(".harness-test-home");
        std::fs::create_dir_all(&fallback).expect("create fallback HOME");
        std::env::set_var("HOME", &fallback);
        fallback
    }

    #[tokio::test]
    async fn initialized_returns_success() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let state = make_test_state(dir.path()).await?;

        let req = RpcRequest {
            jsonrpc: "2.0".to_string(),
            // `initialized` is typically a notification, but we allow an `id` for
            // compatibility and return an empty success response in that case.
            id: Some(serde_json::json!(1)),
            method: Method::Initialized,
        };
        let resp = handle_request(&state, req)
            .await
            .expect("expected response for request with id");

        assert!(
            resp.error.is_none(),
            "initialized should succeed, got error: {:?}",
            resp.error
        );
        assert!(resp.result.is_some(), "initialized must return a result");
        Ok(())
    }

    #[tokio::test]
    async fn initialize_then_initialized_succeeds() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let mut state = make_test_state(dir.path()).await?;
        // Start uninitialised to test the full handshake.
        state.initialized = Arc::new(std::sync::atomic::AtomicBool::new(false));

        // Step 1: initialize
        let init_req = RpcRequest {
            jsonrpc: "2.0".to_string(),
            id: Some(serde_json::json!(1)),
            method: Method::Initialize,
        };
        let init_resp = handle_request(&state, init_req)
            .await
            .expect("expected response for initialize");
        assert!(
            init_resp.error.is_none(),
            "initialize should succeed: {:?}",
            init_resp.error
        );
        let result = init_resp
            .result
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("initialize must return result"))?;
        assert!(
            result["capabilities"].is_object(),
            "capabilities should be present"
        );

        // Step 2: initialized (notification — id is None)
        let ack_req = RpcRequest {
            jsonrpc: "2.0".to_string(),
            id: None,
            method: Method::Initialized,
        };
        let ack_resp = handle_request(&state, ack_req).await;
        assert!(
            ack_resp.is_none(),
            "expected no response for initialized notification, got: {ack_resp:?}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn gc_adopt_response_includes_task_id() -> anyhow::Result<()> {
        use harness_core::{
            Artifact, ArtifactType, Draft, DraftId, DraftStatus, ProjectId, RemediationType,
            Signal, SignalType,
        };

        let dir = tempfile::tempdir()?;
        let state = make_test_state(dir.path()).await?;

        let draft_id = DraftId::new();
        let signal = Signal::new(
            SignalType::RepeatedWarn,
            ProjectId::new(),
            serde_json::json!("test signal"),
            RemediationType::Guard,
        );
        let draft = Draft {
            id: draft_id.clone(),
            status: DraftStatus::Pending,
            signal,
            artifacts: vec![Artifact {
                artifact_type: ArtifactType::Guard,
                target_path: dir.path().join("test-guard.sh"),
                content: "#!/bin/bash\necho ok".to_string(),
            }],
            rationale: "test".to_string(),
            validation: "test".to_string(),
            generated_at: chrono::Utc::now(),
            agent_model: "test".to_string(),
        };
        state.gc_agent.draft_store().save(&draft)?;

        let req = RpcRequest {
            jsonrpc: "2.0".to_string(),
            id: Some(serde_json::json!(1)),
            method: Method::GcAdopt { draft_id },
        };
        let resp = handle_request(&state, req)
            .await
            .expect("expected response for request with id");

        assert!(
            resp.error.is_none(),
            "expected success, got error: {:?}",
            resp.error
        );
        let result = resp
            .result
            .ok_or_else(|| anyhow::anyhow!("missing result"))?;
        assert_eq!(
            result["adopted"],
            serde_json::json!(true),
            "adopted must be true"
        );
        assert_eq!(
            result["task_id"],
            serde_json::json!(null),
            "task_id should be null when no agent is registered"
        );
        Ok(())
    }

    struct MockAgent;

    #[async_trait::async_trait]
    impl harness_core::CodeAgent for MockAgent {
        fn name(&self) -> &str {
            "mock"
        }
        fn capabilities(&self) -> Vec<harness_core::Capability> {
            vec![]
        }
        async fn execute(
            &self,
            _req: harness_core::AgentRequest,
        ) -> harness_core::Result<harness_core::AgentResponse> {
            Ok(harness_core::AgentResponse {
                output: "LGTM".to_string(),
                stderr: String::new(),
                items: vec![],
                token_usage: harness_core::TokenUsage::default(),
                model: "mock".to_string(),
                exit_code: Some(0),
            })
        }
        async fn execute_stream(
            &self,
            _req: harness_core::AgentRequest,
            _tx: tokio::sync::mpsc::Sender<harness_core::StreamItem>,
        ) -> harness_core::Result<()> {
            Ok(())
        }
    }

    struct NonLgtmAgent {
        calls: std::sync::atomic::AtomicUsize,
    }

    impl NonLgtmAgent {
        fn new() -> Self {
            Self {
                calls: std::sync::atomic::AtomicUsize::new(0),
            }
        }
    }

    #[async_trait::async_trait]
    impl harness_core::CodeAgent for NonLgtmAgent {
        fn name(&self) -> &str {
            "non-lgtm"
        }

        fn capabilities(&self) -> Vec<harness_core::Capability> {
            vec![]
        }

        async fn execute(
            &self,
            _req: harness_core::AgentRequest,
        ) -> harness_core::Result<harness_core::AgentResponse> {
            let call = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let output = if call == 0 {
                "Implemented\nPR_URL=https://github.com/example/repo/pull/123".to_string()
            } else {
                "Needs follow-up\nFIXED".to_string()
            };
            Ok(harness_core::AgentResponse {
                output,
                stderr: String::new(),
                items: vec![],
                token_usage: harness_core::TokenUsage::default(),
                model: "mock".to_string(),
                exit_code: Some(0),
            })
        }

        async fn execute_stream(
            &self,
            _req: harness_core::AgentRequest,
            _tx: tokio::sync::mpsc::Sender<harness_core::StreamItem>,
        ) -> harness_core::Result<()> {
            Ok(())
        }
    }

    async fn wait_for_terminal_task(
        state: &AppState,
        task_id: &str,
    ) -> anyhow::Result<crate::task_runner::TaskState> {
        use tokio::time::{sleep, Duration};

        let tid = crate::task_runner::TaskId(task_id.to_string());
        for _ in 0..120 {
            if let Some(task) = state.tasks.get(&tid) {
                if matches!(
                    task.status,
                    crate::task_runner::TaskStatus::Done | crate::task_runner::TaskStatus::Failed
                ) {
                    return Ok(task);
                }
            }
            sleep(Duration::from_millis(25)).await;
        }
        anyhow::bail!("task did not reach terminal state in time");
    }

    async fn run_gc_adopt_and_wait_for_failure_turn(max_rounds: u32) -> anyhow::Result<u32> {
        let home = writable_home();
        let dir = tempfile::Builder::new()
            .prefix("harness-gc-test-")
            .tempdir_in(&home)?;
        let mut config = HarnessConfig::default();
        config.gc.adopt_wait_secs = 0;
        config.gc.adopt_max_rounds = max_rounds;
        config.gc.adopt_turn_timeout_secs = 30;

        let mut registry = AgentRegistry::new("mock");
        registry.register("mock", Arc::new(NonLgtmAgent::new()));
        let state = make_test_state_with_config_and_registry(dir.path(), config, registry).await?;

        let draft_id = harness_core::DraftId::new();
        let signal = harness_core::Signal::new(
            harness_core::SignalType::RepeatedWarn,
            harness_core::ProjectId::new(),
            serde_json::json!("test signal"),
            harness_core::RemediationType::Guard,
        );
        let draft = harness_core::Draft {
            id: draft_id.clone(),
            status: harness_core::DraftStatus::Pending,
            signal,
            artifacts: vec![harness_core::Artifact {
                artifact_type: harness_core::ArtifactType::Guard,
                target_path: dir.path().join("test-guard.sh"),
                content: "#!/bin/bash\necho ok".to_string(),
            }],
            rationale: "test".to_string(),
            validation: "test".to_string(),
            generated_at: chrono::Utc::now(),
            agent_model: "test".to_string(),
        };
        state.gc_agent.draft_store().save(&draft)?;

        let req = RpcRequest {
            jsonrpc: "2.0".to_string(),
            id: Some(serde_json::json!(1)),
            method: Method::GcAdopt { draft_id },
        };
        let resp = handle_request(&state, req)
            .await
            .expect("expected response for request with id");
        assert!(
            resp.error.is_none(),
            "expected success, got error: {:?}",
            resp.error
        );

        let result = resp
            .result
            .ok_or_else(|| anyhow::anyhow!("missing result"))?;
        let task_id = result["task_id"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("task_id should be a string"))?;

        let task = wait_for_terminal_task(&state, task_id).await?;
        assert!(
            matches!(task.status, crate::task_runner::TaskStatus::Failed),
            "expected task to fail after exhausting rounds, got {:?}",
            task.status
        );
        Ok(task.turn)
    }

    #[tokio::test]
    async fn gc_adopt_spawns_task_when_agent_registered() -> anyhow::Result<()> {
        use harness_core::{
            Artifact, ArtifactType, Draft, DraftId, DraftStatus, ProjectId, RemediationType,
            Signal, SignalType,
        };

        let dir = tempfile::tempdir()?;
        let mut registry = AgentRegistry::new("mock");
        registry.register("mock", Arc::new(MockAgent));
        let state = make_test_state_with_registry(dir.path(), registry).await?;

        let draft_id = DraftId::new();
        let signal = Signal::new(
            SignalType::RepeatedWarn,
            ProjectId::new(),
            serde_json::json!("test signal"),
            RemediationType::Guard,
        );
        let artifact_path = dir.path().join("test-guard.sh");
        let draft = Draft {
            id: draft_id.clone(),
            status: DraftStatus::Pending,
            signal,
            artifacts: vec![Artifact {
                artifact_type: ArtifactType::Guard,
                target_path: artifact_path,
                content: "#!/bin/bash\necho ok".to_string(),
            }],
            rationale: "test".to_string(),
            validation: "test".to_string(),
            generated_at: chrono::Utc::now(),
            agent_model: "test".to_string(),
        };
        state.gc_agent.draft_store().save(&draft)?;

        let req = RpcRequest {
            jsonrpc: "2.0".to_string(),
            id: Some(serde_json::json!(1)),
            method: Method::GcAdopt { draft_id },
        };
        let resp = handle_request(&state, req)
            .await
            .expect("expected response for request with id");

        assert!(
            resp.error.is_none(),
            "expected success, got error: {:?}",
            resp.error
        );
        let result = resp
            .result
            .ok_or_else(|| anyhow::anyhow!("missing result"))?;
        assert_eq!(
            result["adopted"],
            serde_json::json!(true),
            "adopted must be true"
        );
        let task_id = result["task_id"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("task_id should be a string"))?;
        assert!(!task_id.is_empty(), "task_id should be non-empty");
        let tid = crate::task_runner::TaskId(task_id.to_string());
        let task = state.tasks.get(&tid);
        assert!(task.is_some(), "task should exist in the task store");
        Ok(())
    }

    #[tokio::test]
    async fn gc_adopt_schedule_changes_with_gc_config() -> anyhow::Result<()> {
        let short_max_rounds = 1;
        let long_max_rounds = 3;
        let short_schedule_turn = run_gc_adopt_and_wait_for_failure_turn(short_max_rounds).await?;
        let long_schedule_turn = run_gc_adopt_and_wait_for_failure_turn(long_max_rounds).await?;

        assert_eq!(
            short_schedule_turn,
            short_max_rounds + 1,
            "max_rounds=1 should end at turn 2"
        );
        assert_eq!(
            long_schedule_turn,
            long_max_rounds + 1,
            "max_rounds=3 should end at turn 4"
        );
        assert!(
            long_schedule_turn > short_schedule_turn,
            "larger max_rounds should produce a longer schedule"
        );
        Ok(())
    }

    #[tokio::test]
    async fn rule_check_logs_no_violations_when_no_guards_loaded() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let state = make_test_state(dir.path()).await?;
        let home = writable_home();
        let proj_dir = tempfile::Builder::new()
            .prefix("harness-test-")
            .tempdir_in(&home)?;

        let req = RpcRequest {
            jsonrpc: "2.0".to_string(),
            id: Some(serde_json::json!(1)),
            method: Method::RuleCheck {
                project_root: proj_dir.path().to_path_buf(),
                files: None,
            },
        };
        let resp = handle_request(&state, req)
            .await
            .expect("expected response for request with id");

        assert!(resp.error.is_none(), "expected success: {:?}", resp.error);
        let violations: Vec<serde_json::Value> = serde_json::from_value(
            resp.result
                .ok_or_else(|| anyhow::anyhow!("missing result"))?,
        )?;
        assert!(
            violations.is_empty(),
            "no guards loaded, violations must be empty"
        );

        let events = state.events.query(&harness_core::EventFilters {
            hook: Some("rule_check".to_string()),
            ..Default::default()
        })?;
        assert!(
            events.is_empty(),
            "no events should be logged when there are no violations"
        );
        Ok(())
    }

    #[tokio::test]
    async fn metrics_query_counts_rule_violations_from_events() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let state = make_test_state(dir.path()).await?;

        let session_id = harness_core::SessionId::new();
        // The metrics_query handler scopes violation counts by the latest
        // rule_scan session, so we must log a rule_scan summary event first.
        let scan_event = harness_core::Event::new(
            session_id.clone(),
            "rule_scan",
            "RuleEngine",
            harness_core::Decision::Block,
        );
        state.events.log(&scan_event)?;
        for _ in 0..5 {
            let event = harness_core::Event::new(
                session_id.clone(),
                "rule_check",
                "SEC-01",
                harness_core::Decision::Block,
            );
            state.events.log(&event)?;
        }

        let req = RpcRequest {
            jsonrpc: "2.0".to_string(),
            id: Some(serde_json::json!(1)),
            method: Method::MetricsQuery {
                filters: harness_core::MetricFilters::default(),
            },
        };
        let resp = handle_request(&state, req)
            .await
            .expect("expected response for request with id");

        assert!(resp.error.is_none(), "expected success: {:?}", resp.error);
        let result = resp
            .result
            .ok_or_else(|| anyhow::anyhow!("missing result"))?;
        let coverage = result["dimensions"]["coverage"]
            .as_f64()
            .ok_or_else(|| anyhow::anyhow!("missing coverage"))?;
        assert!(
            coverage < 100.0,
            "coverage should degrade with violations, got {coverage}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn metrics_query_sees_violations_written_via_handler_entry() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let state = make_test_state(dir.path()).await?;
        let project_root = dir.path().join("project");
        std::fs::create_dir_all(&project_root)?;
        let violations = vec![
            harness_core::Violation {
                rule_id: harness_core::RuleId::from_str("SEC-01"),
                file: std::path::PathBuf::from("src/lib.rs"),
                line: Some(7),
                message: "critical issue".to_string(),
                severity: harness_core::Severity::Critical,
            },
            harness_core::Violation {
                rule_id: harness_core::RuleId::from_str("U-01"),
                file: std::path::PathBuf::from("src/main.rs"),
                line: None,
                message: "style issue".to_string(),
                severity: harness_core::Severity::Low,
            },
        ];
        state.events.persist_rule_scan(&project_root, &violations);

        let req = RpcRequest {
            jsonrpc: "2.0".to_string(),
            id: Some(serde_json::json!(1)),
            method: Method::MetricsQuery {
                filters: harness_core::MetricFilters::default(),
            },
        };
        let resp = handle_request(&state, req)
            .await
            .expect("expected response for request with id");

        assert!(resp.error.is_none(), "expected success: {:?}", resp.error);
        let result = resp
            .result
            .ok_or_else(|| anyhow::anyhow!("missing result"))?;
        let coverage = result["dimensions"]["coverage"]
            .as_f64()
            .ok_or_else(|| anyhow::anyhow!("missing coverage"))?;
        assert!(
            coverage < 100.0,
            "coverage should degrade with persisted violations, got {coverage}"
        );

        let events = state.events.query(&harness_core::EventFilters::default())?;
        let latest_scan = events
            .iter()
            .rev()
            .find(|event| event.hook == "rule_scan")
            .ok_or_else(|| anyhow::anyhow!("missing rule_scan anchor event"))?;
        let linked_checks = events
            .iter()
            .filter(|event| {
                event.hook == "rule_check" && event.session_id == latest_scan.session_id
            })
            .count();
        assert_eq!(linked_checks, violations.len());

        Ok(())
    }

    #[tokio::test]
    async fn thread_start_persists_to_db() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let state = make_test_state(dir.path()).await?;
        let home = writable_home();
        let proj_dir = tempfile::Builder::new()
            .prefix("harness-test-")
            .tempdir_in(&home)?;
        let canonical_proj = proj_dir.path().canonicalize()?;

        let req = RpcRequest {
            jsonrpc: "2.0".to_string(),
            id: Some(serde_json::json!(1)),
            method: Method::ThreadStart {
                cwd: proj_dir.path().to_path_buf(),
            },
        };
        let resp = handle_request(&state, req)
            .await
            .expect("expected response for request with id");

        assert!(
            resp.error.is_none(),
            "expected success, got error: {:?}",
            resp.error
        );
        let thread_id_str = resp.result.unwrap()["thread_id"]
            .as_str()
            .unwrap()
            .to_string();

        let db = state.thread_db.as_ref().unwrap();
        let thread = db
            .get(&thread_id_str)
            .await?
            .expect("thread should be in DB");
        assert_eq!(thread.project_root, canonical_proj);
        Ok(())
    }

    #[tokio::test]
    async fn thread_start_invalid_root_returns_validation_error() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let state = make_test_state(dir.path()).await?;

        let req = RpcRequest {
            jsonrpc: "2.0".to_string(),
            id: Some(serde_json::json!(1)),
            method: Method::ThreadStart {
                cwd: dir.path().join("missing-project-root"),
            },
        };
        let resp = handle_request(&state, req)
            .await
            .expect("expected response for request with id");

        let error = resp.error.ok_or_else(|| anyhow::anyhow!("missing error"))?;
        assert_eq!(error.code, harness_protocol::VALIDATION);
        Ok(())
    }

    #[tokio::test]
    async fn gc_run_without_default_agent_returns_agent_unavailable() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let state = make_test_state(dir.path()).await?;

        let req = RpcRequest {
            jsonrpc: "2.0".to_string(),
            id: Some(serde_json::json!(1)),
            method: Method::GcRun { project_id: None },
        };
        let resp = handle_request(&state, req)
            .await
            .expect("expected response for request with id");

        let error = resp.error.ok_or_else(|| anyhow::anyhow!("missing error"))?;
        assert_eq!(error.code, harness_protocol::AGENT_UNAVAILABLE);
        Ok(())
    }

    #[tokio::test]
    async fn gc_adopt_missing_draft_returns_not_found() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let state = make_test_state(dir.path()).await?;

        let req = RpcRequest {
            jsonrpc: "2.0".to_string(),
            id: Some(serde_json::json!(1)),
            method: Method::GcAdopt {
                draft_id: harness_core::DraftId::new(),
            },
        };
        let resp = handle_request(&state, req)
            .await
            .expect("expected response for request with id");

        let error = resp.error.ok_or_else(|| anyhow::anyhow!("missing error"))?;
        assert_eq!(error.code, harness_protocol::NOT_FOUND);
        Ok(())
    }

    #[tokio::test]
    async fn pre_init_request_rejected() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let mut state = make_test_state(dir.path()).await?;
        state.initialized = Arc::new(std::sync::atomic::AtomicBool::new(false));

        let req = RpcRequest {
            jsonrpc: "2.0".to_string(),
            id: Some(serde_json::json!(1)),
            method: Method::ThreadList,
        };
        let resp = handle_request(&state, req)
            .await
            .expect("should return error response");
        assert!(resp.error.is_some(), "pre-init request should be rejected");
        assert_eq!(resp.error.unwrap().code, harness_protocol::INVALID_REQUEST);
        Ok(())
    }

    #[tokio::test]
    async fn double_initialize_rejected() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let state = make_test_state(dir.path()).await?;
        // state.initialized is already true from make_test_state

        let req = RpcRequest {
            jsonrpc: "2.0".to_string(),
            id: Some(serde_json::json!(1)),
            method: Method::Initialize,
        };
        let resp = handle_request(&state, req)
            .await
            .expect("should return error response");
        assert!(resp.error.is_some(), "double init should be rejected");
        Ok(())
    }

    #[tokio::test]
    async fn handshake_unlocks_methods() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let mut state = make_test_state(dir.path()).await?;
        state.initialized = Arc::new(std::sync::atomic::AtomicBool::new(false));

        // Before handshake: ThreadList rejected
        let req = RpcRequest {
            jsonrpc: "2.0".to_string(),
            id: Some(serde_json::json!(1)),
            method: Method::ThreadList,
        };
        let resp = handle_request(&state, req).await.unwrap();
        assert!(resp.error.is_some());

        // Initialize
        let req = RpcRequest {
            jsonrpc: "2.0".to_string(),
            id: Some(serde_json::json!(2)),
            method: Method::Initialize,
        };
        let resp = handle_request(&state, req).await.unwrap();
        assert!(resp.error.is_none(), "initialize should succeed");

        // Initialized (complete handshake)
        let req = RpcRequest {
            jsonrpc: "2.0".to_string(),
            id: None,
            method: Method::Initialized,
        };
        handle_request(&state, req).await;

        // After handshake: ThreadList works
        let req = RpcRequest {
            jsonrpc: "2.0".to_string(),
            id: Some(serde_json::json!(3)),
            method: Method::ThreadList,
        };
        let resp = handle_request(&state, req).await.unwrap();
        assert!(resp.error.is_none(), "post-init request should work");
        Ok(())
    }
}
