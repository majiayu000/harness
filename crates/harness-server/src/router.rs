use crate::handlers;
use crate::http::AppState;
use harness_protocol::{Method, RpcRequest, RpcResponse};

/// Route a JSON-RPC request to the appropriate handler.
pub async fn handle_request(state: &AppState, req: RpcRequest) -> Option<RpcResponse> {
    let id = req.id.clone();

    match req.method {
        // === Initialization ===
        Method::Initialize => Some(handlers::thread::initialize(id).await),
        Method::Initialized => {
            handlers::thread::initialized().await;
            // `initialized` is a notification in JSON-RPC (no response when `id` is absent).
            if id.is_none() {
                None
            } else {
                Some(RpcResponse::success(id, serde_json::json!({})))
            }
        }

        // === Thread management ===
        Method::ThreadStart { cwd } => {
            Some(handlers::thread::thread_start(state, id, cwd).await)
        }
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
        Method::SkillList { query } => {
            Some(handlers::skills::skill_list(state, id, query).await)
        }
        Method::SkillGet { skill_id } => {
            Some(handlers::skills::skill_get(state, id, skill_id).await)
        }
        Method::SkillDelete { skill_id } => {
            Some(handlers::skills::skill_delete(state, id, skill_id).await)
        }

        // === Events / Metrics ===
        Method::EventLog { event } => {
            Some(handlers::observe::event_log(state, id, event).await)
        }
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
        Method::GcRun { project_id: _ } => {
            Some(handlers::gc::gc_run(state, id).await)
        }
        Method::GcStatus => Some(handlers::gc::gc_status(state, id).await),
        Method::GcDrafts { project_id: _ } => {
            Some(handlers::gc::gc_drafts(state, id).await)
        }
        Method::GcAdopt { draft_id } => {
            Some(handlers::gc::gc_adopt(state, id, draft_id).await)
        }
        Method::GcReject { draft_id, reason } => {
            Some(handlers::gc::gc_reject(state, id, draft_id, reason).await)
        }

        // === ExecPlan ===
        Method::ExecPlanInit {
            spec,
            project_root,
        } => Some(handlers::exec::exec_plan_init(state, id, spec, project_root).await),
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
        Method::Preflight { project_root, task_description } => {
            Some(handlers::preflight::preflight(state, id, project_root, task_description).await)
        }
        Method::CrossReview { project_root, target, max_rounds } => {
            Some(handlers::cross_review::cross_review(state, id, project_root, target, max_rounds).await)
        }
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
        make_test_state_with_registry(dir, AgentRegistry::new("test")).await
    }

    async fn make_test_state_with_registry(
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
            harness_gc::signal_detector::SignalThresholds::default(),
            harness_core::ProjectId::new(),
        );
        let draft_store = harness_gc::DraftStore::new(dir)?;
        let gc_agent = Arc::new(harness_gc::GcAgent::new(
            harness_gc::gc_agent::GcConfig::default(),
            signal_detector,
            draft_store,
        ));
        let thread_db =
            crate::thread_db::ThreadDb::open(&dir.join("threads.db")).await?;
        let (notification_tx, _) = tokio::sync::broadcast::channel(64);
        Ok(AppState {
            server,
            tasks,
            skills: Arc::new(RwLock::new(harness_skills::SkillStore::new())),
            rules: Arc::new(RwLock::new(harness_rules::engine::RuleEngine::new())),
            events,
            gc_agent,
            plans: Arc::new(RwLock::new(std::collections::HashMap::new())),
            thread_db: Some(thread_db),
            interceptors: vec![],
            notification_tx,
            notify_tx: None,
        })
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
        let state = make_test_state(dir.path()).await?;

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
        assert!(result["capabilities"].is_object(), "capabilities should be present");

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
            Artifact, ArtifactType, Draft, DraftId, DraftStatus, ProjectId,
            RemediationType, Signal, SignalType,
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
        let result =
            resp.result
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

    #[tokio::test]
    async fn gc_adopt_spawns_task_when_agent_registered() -> anyhow::Result<()> {
        use harness_core::{
            Artifact, ArtifactType, Draft, DraftId, DraftStatus, ProjectId,
            RemediationType, Signal, SignalType,
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
        let result =
            resp.result
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
    async fn rule_check_logs_no_violations_when_no_guards_loaded() -> anyhow::Result<()>
    {
        let dir = tempfile::tempdir()?;
        let state = make_test_state(dir.path()).await?;
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
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

        assert!(
            resp.error.is_none(),
            "expected success: {:?}",
            resp.error
        );
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

        assert!(
            resp.error.is_none(),
            "expected success: {:?}",
            resp.error
        );
        let result =
            resp.result
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
    async fn thread_start_persists_to_db() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let state = make_test_state(dir.path()).await?;
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
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
}
