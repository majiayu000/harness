use crate::handlers;
use crate::http::AppState;
use harness_protocol::{Method, RpcRequest, RpcResponse};

/// Route a JSON-RPC request to the appropriate handler.
pub async fn handle_request(state: &AppState, req: RpcRequest) -> RpcResponse {
    let id = req.id.clone();

    match req.method {
        // === Initialization ===
        Method::Initialize => handlers::thread::initialize(id).await,

        // === Thread management ===
        Method::ThreadStart { cwd } => {
            handlers::thread::thread_start(state, id, cwd).await
        }
        Method::ThreadList => handlers::thread::thread_list(state, id).await,
        Method::ThreadDelete { thread_id } => {
            handlers::thread::thread_delete(state, id, thread_id).await
        }
        Method::ThreadResume { thread_id } => {
            handlers::thread::thread_resume(state, id, thread_id).await
        }
        Method::ThreadFork {
            thread_id,
            from_turn,
        } => handlers::thread::thread_fork(state, id, thread_id, from_turn).await,
        Method::ThreadCompact { thread_id } => {
            handlers::thread::thread_compact(state, id, thread_id).await
        }

        // === Turn control ===
        Method::TurnStart { thread_id, input } => {
            handlers::thread::turn_start(state, id, thread_id, input).await
        }
        Method::TurnCancel { turn_id } => {
            handlers::thread::turn_cancel(state, id, turn_id).await
        }
        Method::TurnStatus { turn_id } => {
            handlers::thread::turn_status(state, id, turn_id).await
        }
        Method::TurnSteer {
            turn_id,
            instruction,
        } => handlers::thread::turn_steer(state, id, turn_id, instruction).await,

        // === Skills ===
        Method::SkillCreate { name, content } => {
            handlers::skills::skill_create(state, id, name, content).await
        }
        Method::SkillList { query } => {
            handlers::skills::skill_list(state, id, query).await
        }
        Method::SkillGet { skill_id } => {
            handlers::skills::skill_get(state, id, skill_id).await
        }
        Method::SkillDelete { skill_id } => {
            handlers::skills::skill_delete(state, id, skill_id).await
        }

        // === Events / Metrics ===
        Method::EventLog { event } => {
            handlers::observe::event_log(state, id, event).await
        }
        Method::EventQuery { filters } => {
            handlers::observe::event_query(state, id, filters).await
        }
        Method::MetricsCollect { project_root } => {
            handlers::observe::metrics_collect(state, id, project_root).await
        }
        Method::MetricsQuery { filters } => {
            handlers::observe::metrics_query(state, id, filters).await
        }

        // === Rules ===
        Method::RuleLoad { project_root } => {
            handlers::rules::rule_load(state, id, project_root).await
        }
        Method::RuleCheck {
            project_root,
            files,
        } => handlers::rules::rule_check(state, id, project_root, files).await,

        // === GC ===
        Method::GcRun { project_id: _ } => {
            handlers::gc::gc_run(state, id).await
        }
        Method::GcStatus => handlers::gc::gc_status(state, id).await,
        Method::GcDrafts { project_id: _ } => {
            handlers::gc::gc_drafts(state, id).await
        }
        Method::GcAdopt { draft_id } => {
            handlers::gc::gc_adopt(state, id, draft_id).await
        }
        Method::GcReject { draft_id, reason } => {
            handlers::gc::gc_reject(state, id, draft_id, reason).await
        }

        // === ExecPlan ===
        Method::ExecPlanInit {
            spec,
            project_root,
        } => handlers::exec::exec_plan_init(state, id, spec, project_root).await,
        Method::ExecPlanStatus { plan_id } => {
            handlers::exec::exec_plan_status(state, id, plan_id).await
        }
        Method::ExecPlanUpdate { plan_id, updates } => {
            handlers::exec::exec_plan_update(state, id, plan_id, updates).await
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
        })
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
        let resp = handle_request(&state, req).await;

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
        let resp = handle_request(&state, req).await;

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

        let req = RpcRequest {
            jsonrpc: "2.0".to_string(),
            id: Some(serde_json::json!(1)),
            method: Method::RuleCheck {
                project_root: dir.path().to_path_buf(),
                files: None,
            },
        };
        let resp = handle_request(&state, req).await;

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
        let resp = handle_request(&state, req).await;

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

        let req = RpcRequest {
            jsonrpc: "2.0".to_string(),
            id: Some(serde_json::json!(1)),
            method: Method::ThreadStart {
                cwd: std::path::PathBuf::from("/test/project"),
            },
        };
        let resp = handle_request(&state, req).await;

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
        assert_eq!(
            thread.project_root,
            std::path::PathBuf::from("/test/project")
        );
        Ok(())
    }
}
