use crate::http::AppState;
use harness_core::ThreadId;
use harness_protocol::{Method, RpcRequest, RpcResponse, INTERNAL_ERROR};

/// Persist an existing thread to the optional ThreadDb after a mutation.
async fn persist_thread(state: &AppState, thread_id: &ThreadId) {
    if let Some(db) = &state.thread_db {
        if let Some(thread) = state.server.thread_manager.get_thread(thread_id) {
            if let Err(e) = db.update(&thread).await {
                tracing::warn!("thread_db persist failed: {e}");
            }
        }
    }
}

/// Insert a newly created thread into the optional ThreadDb.
async fn persist_thread_insert(state: &AppState, thread_id: &ThreadId) {
    if let Some(db) = &state.thread_db {
        if let Some(thread) = state.server.thread_manager.get_thread(thread_id) {
            if let Err(e) = db.insert(&thread).await {
                tracing::warn!("thread_db insert failed: {e}");
            }
        }
    }
}

/// Route a JSON-RPC request to the appropriate handler.
pub async fn handle_request(state: &AppState, req: RpcRequest) -> RpcResponse {
    let id = req.id.clone();
    let server = &state.server;

    match req.method {
        Method::Initialize => {
            RpcResponse::success(id, serde_json::json!({
                "name": "harness",
                "version": env!("CARGO_PKG_VERSION"),
                "capabilities": {
                    "threads": true,
                    "gc": true,
                    "skills": true,
                    "rules": true,
                    "exec_plan": true,
                    "observe": true,
                }
            }))
        }

        Method::ThreadStart { cwd } => {
            let thread_id = server.thread_manager.start_thread(cwd);
            persist_thread_insert(state, &thread_id).await;
            RpcResponse::success(id, serde_json::json!({ "thread_id": thread_id }))
        }

        Method::ThreadList => {
            let threads = server.thread_manager.list_threads();
            RpcResponse::success(id, serde_json::to_value(threads).unwrap_or_default())
        }

        Method::ThreadDelete { thread_id } => {
            let deleted = server.thread_manager.delete_thread(&thread_id);
            if deleted {
                if let Some(db) = &state.thread_db {
                    if let Err(e) = db.delete(thread_id.as_str()).await {
                        tracing::warn!("thread_db delete failed: {e}");
                    }
                }
            }
            RpcResponse::success(id, serde_json::json!({ "deleted": deleted }))
        }

        Method::TurnStart { thread_id, input } => {
            let agent_id = harness_core::AgentId::from_str(
                &server.config.agents.default_agent,
            );
            match server.thread_manager.start_turn(&thread_id, input, agent_id) {
                Ok(turn_id) => {
                    persist_thread(state, &thread_id).await;
                    RpcResponse::success(id, serde_json::json!({ "turn_id": turn_id }))
                }
                Err(e) => RpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
            }
        }

        Method::TurnCancel { turn_id } => {
            match server.thread_manager.find_thread_for_turn(&turn_id) {
                Some(thread_id) => {
                    match server.thread_manager.cancel_turn(&thread_id, &turn_id) {
                        Ok(()) => {
                            persist_thread(state, &thread_id).await;
                            RpcResponse::success(id, serde_json::json!({ "cancelled": true }))
                        }
                        Err(e) => RpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
                    }
                }
                None => RpcResponse::error(id, INTERNAL_ERROR, "turn not found in any thread"),
            }
        }

        Method::TurnStatus { turn_id } => {
            match server.thread_manager.find_thread_for_turn(&turn_id) {
                Some(thread_id) => {
                    if let Some(thread) = server.thread_manager.get_thread(&thread_id) {
                        if let Some(turn) = thread.turns.iter().find(|t| t.id == turn_id) {
                            match serde_json::to_value(turn) {
                                Ok(v) => RpcResponse::success(id, v),
                                Err(e) => RpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
                            }
                        } else {
                            RpcResponse::error(id, INTERNAL_ERROR, "turn not found")
                        }
                    } else {
                        RpcResponse::error(id, INTERNAL_ERROR, "thread not found")
                    }
                }
                None => RpcResponse::error(id, INTERNAL_ERROR, "turn not found in any thread"),
            }
        }

        Method::TurnSteer { turn_id, instruction } => {
            match server.thread_manager.find_thread_for_turn(&turn_id) {
                Some(thread_id) => {
                    match server.thread_manager.steer_turn(&thread_id, &turn_id, instruction) {
                        Ok(()) => {
                            persist_thread(state, &thread_id).await;
                            RpcResponse::success(id, serde_json::json!({ "steered": true }))
                        }
                        Err(e) => RpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
                    }
                }
                None => RpcResponse::error(id, INTERNAL_ERROR, "turn not found in any thread"),
            }
        }

        // === Thread advanced ===

        Method::ThreadResume { thread_id } => {
            match server.thread_manager.resume_thread(&thread_id) {
                Ok(()) => {
                    persist_thread(state, &thread_id).await;
                    RpcResponse::success(id, serde_json::json!({ "resumed": true }))
                }
                Err(e) => RpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
            }
        }

        Method::ThreadFork { thread_id, from_turn } => {
            match server.thread_manager.fork_thread(&thread_id, from_turn.as_ref()) {
                Ok(new_id) => {
                    persist_thread_insert(state, &new_id).await;
                    RpcResponse::success(id, serde_json::json!({ "thread_id": new_id }))
                }
                Err(e) => RpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
            }
        }

        Method::ThreadCompact { thread_id } => {
            match server.thread_manager.compact_thread(&thread_id) {
                Ok(()) => {
                    persist_thread(state, &thread_id).await;
                    RpcResponse::success(id, serde_json::json!({ "compacted": true }))
                }
                Err(e) => RpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
            }
        }

        // === Skills ===

        Method::SkillCreate { name, content } => {
            let mut skills = state.skills.write().await;
            let skill = skills.create(name, content).clone();
            match serde_json::to_value(&skill) {
                Ok(v) => RpcResponse::success(id, v),
                Err(e) => RpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
            }
        }

        Method::SkillList { query } => {
            let skills = state.skills.read().await;
            let result = match query {
                Some(q) => skills.search(&q).into_iter().cloned().collect::<Vec<_>>(),
                None => skills.list().to_vec(),
            };
            match serde_json::to_value(&result) {
                Ok(v) => RpcResponse::success(id, v),
                Err(e) => RpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
            }
        }

        Method::SkillGet { skill_id } => {
            let skills = state.skills.read().await;
            match skills.get(&skill_id) {
                Some(skill) => match serde_json::to_value(skill) {
                    Ok(v) => RpcResponse::success(id, v),
                    Err(e) => RpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
                },
                None => RpcResponse::error(id, INTERNAL_ERROR, "skill not found"),
            }
        }

        Method::SkillDelete { skill_id } => {
            let mut skills = state.skills.write().await;
            let deleted = skills.delete(&skill_id);
            RpcResponse::success(id, serde_json::json!({ "deleted": deleted }))
        }

        // === Events / Metrics ===

        Method::EventLog { event } => {
            match state.events.log(&event) {
                Ok(event_id) => RpcResponse::success(id, serde_json::json!({ "logged": true, "event_id": event_id })),
                Err(e) => RpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
            }
        }

        Method::EventQuery { filters } => {
            match state.events.query(&filters) {
                Ok(events) => match serde_json::to_value(&events) {
                    Ok(v) => RpcResponse::success(id, v),
                    Err(e) => RpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
                },
                Err(e) => RpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
            }
        }

        Method::MetricsCollect { project_root } => {
            let events = state.events.query(&harness_core::EventFilters::default());
            match events {
                Ok(evts) => {
                    let violation_count = {
                        let rules = state.rules.read().await;
                        rules.scan(&project_root).await.map(|v| v.len()).unwrap_or(0)
                    };
                    let report = harness_observe::QualityGrader::grade(&evts, violation_count);
                    match serde_json::to_value(&report) {
                        Ok(v) => RpcResponse::success(id, v),
                        Err(e) => RpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
                    }
                }
                Err(e) => RpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
            }
        }

        Method::MetricsQuery { filters } => {
            let event_filters = harness_core::EventFilters {
                since: filters.since,
                until: filters.until,
                ..Default::default()
            };
            let events = state.events.query(&event_filters);
            match events {
                Ok(evts) => {
                    // Count rule violations persisted by RuleCheck handler.
                    let violation_count = evts
                        .iter()
                        .filter(|e| {
                            e.hook == "rule_check"
                                && matches!(
                                    e.decision,
                                    harness_core::Decision::Block | harness_core::Decision::Warn
                                )
                        })
                        .count();
                    let report = harness_observe::QualityGrader::grade(&evts, violation_count);
                    match serde_json::to_value(&report) {
                        Ok(v) => RpcResponse::success(id, v),
                        Err(e) => RpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
                    }
                }
                Err(e) => RpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
            }
        }

        // === Rules ===

        Method::RuleLoad { project_root } => {
            let mut rules = state.rules.write().await;
            match rules.load(&project_root) {
                Ok(()) => {
                    let count = rules.rules().len();
                    RpcResponse::success(id, serde_json::json!({ "rules_count": count }))
                }
                Err(e) => RpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
            }
        }

        Method::RuleCheck { project_root, files } => {
            let result = {
                let rules = state.rules.read().await;
                match files {
                    Some(f) => rules.scan_files(&project_root, &f).await,
                    None => rules.scan(&project_root).await,
                }
            };
            match result {
                Ok(violations) => {
                    // Persist each violation as an Event so the observability
                    // pipeline (MetricsQuery, GcRun) can see them.
                    let session_id = harness_core::SessionId::new();
                    for violation in &violations {
                        let decision = match violation.severity {
                            harness_core::Severity::Critical | harness_core::Severity::High => {
                                harness_core::Decision::Block
                            }
                            harness_core::Severity::Medium => harness_core::Decision::Warn,
                            harness_core::Severity::Low => harness_core::Decision::Pass,
                        };
                        let mut event = harness_core::Event::new(
                            session_id.clone(),
                            "rule_check",
                            violation.rule_id.as_str(),
                            decision,
                        );
                        event.reason = Some(violation.message.clone());
                        event.detail = Some(format!(
                            "{}:{}",
                            violation.file.display(),
                            violation.line.map(|l| l.to_string()).unwrap_or_default()
                        ));
                        if let Err(e) = state.events.log(&event) {
                            tracing::warn!("failed to log rule violation event: {e}");
                        }
                    }
                    match serde_json::to_value(&violations) {
                        Ok(v) => RpcResponse::success(id, v),
                        Err(e) => RpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
                    }
                }
                Err(e) => RpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
            }
        }

        // === GC ===

        Method::GcRun { project_id: _ } => {
            let events = match state.events.query(&harness_core::EventFilters::default()) {
                Ok(e) => e,
                Err(e) => return RpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
            };
            let project_root = std::path::PathBuf::from(".");
            let violations = {
                let rules = state.rules.read().await;
                rules.scan(&project_root).await.unwrap_or_default()
            };
            let project = harness_core::Project::from_path(project_root);
            let agent = match server.agent_registry.default_agent() {
                Some(a) => a,
                None => return RpcResponse::error(id, INTERNAL_ERROR, "no agent registered"),
            };
            match state.gc_agent.run(&project, &events, &violations, agent.as_ref()).await {
                Ok(report) => match serde_json::to_value(&report) {
                    Ok(v) => RpcResponse::success(id, v),
                    Err(e) => RpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
                },
                Err(e) => RpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
            }
        }

        Method::GcStatus => {
            match state.gc_agent.drafts() {
                Ok(drafts) => RpcResponse::success(id, serde_json::json!({ "draft_count": drafts.len() })),
                Err(e) => RpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
            }
        }

        Method::GcDrafts { project_id: _ } => {
            match state.gc_agent.drafts() {
                Ok(drafts) => match serde_json::to_value(&drafts) {
                    Ok(v) => RpcResponse::success(id, v),
                    Err(e) => RpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
                },
                Err(e) => RpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
            }
        }

        Method::GcAdopt { draft_id } => {
            // Fetch draft and artifact paths before adopting.
            let draft = match state.gc_agent.draft_store().get(&draft_id) {
                Ok(Some(d)) => d,
                Ok(None) => {
                    return RpcResponse::error(
                        id,
                        INTERNAL_ERROR,
                        format!("draft {} not found", draft_id),
                    );
                }
                Err(e) => return RpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
            };
            let artifact_paths: Vec<String> = draft
                .artifacts
                .iter()
                .map(|a| a.target_path.display().to_string())
                .collect();

            match state.gc_agent.adopt(&draft_id) {
                Ok(()) => {
                    if artifact_paths.is_empty() {
                        return RpcResponse::success(
                            id,
                            serde_json::json!({ "adopted": true, "task_id": null }),
                        );
                    }
                    // Spawn a task to commit the artifacts and open a PR.
                    const GC_ADOPT_WAIT_SECS: u64 = 120;
                    const GC_ADOPT_MAX_ROUNDS: u32 = 3;
                    const GC_ADOPT_TURN_TIMEOUT_SECS: u64 = 600;
                    let task_id = if let Some(agent) = server.agent_registry.default_agent() {
                        let paths_list = artifact_paths.join(", ");
                        let prompt = format!(
                            "GC drafted the following files: {paths_list}. \
                             Review these changes, create a branch named gc/{draft_id}, \
                             commit, push, and open a PR. \
                             Print PR_URL=<url> on the last line."
                        );
                        let req = crate::task_runner::CreateTaskRequest {
                            prompt: Some(prompt),
                            issue: None,
                            pr: None,
                            project: None,
                            wait_secs: GC_ADOPT_WAIT_SECS,
                            max_rounds: GC_ADOPT_MAX_ROUNDS,
                            turn_timeout_secs: GC_ADOPT_TURN_TIMEOUT_SECS,
                        };
                        let tid = crate::task_runner::spawn_task(
                            state.tasks.clone(),
                            agent,
                            state.skills.clone(),
                            state.events.clone(),
                            req,
                        )
                        .await;
                        Some(tid.0)
                    } else {
                        None
                    };
                    RpcResponse::success(id, serde_json::json!({ "adopted": true, "task_id": task_id }))
                }
                Err(e) => RpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
            }
        }

        Method::GcReject { draft_id, reason } => {
            match state.gc_agent.reject(&draft_id, reason.as_deref()) {
                Ok(()) => RpcResponse::success(id, serde_json::json!({ "rejected": true })),
                Err(e) => RpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
            }
        }

        // === ExecPlan ===

        Method::ExecPlanInit { spec, project_root } => {
            match harness_exec::ExecPlan::from_spec(&spec, &project_root) {
                Ok(plan) => {
                    let plan_id = plan.id.clone();
                    let mut plans = state.plans.write().await;
                    plans.insert(plan_id.clone(), plan);
                    RpcResponse::success(id, serde_json::json!({ "plan_id": plan_id }))
                }
                Err(e) => RpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
            }
        }

        Method::ExecPlanStatus { plan_id } => {
            let plans = state.plans.read().await;
            match plans.get(&plan_id) {
                Some(plan) => match serde_json::to_value(plan) {
                    Ok(v) => RpcResponse::success(id, v),
                    Err(e) => RpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
                },
                None => RpcResponse::error(id, INTERNAL_ERROR, "plan not found"),
            }
        }

        // === VibeGuard: Cross-Review ===

        Method::CrossReview { project_root, target, max_rounds } => {
            crate::handlers::cross_review::cross_review(state, id, project_root, target, max_rounds).await
        }

        Method::ExecPlanUpdate { plan_id, updates } => {
            let mut plans = state.plans.write().await;
            match plans.get_mut(&plan_id) {
                Some(plan) => {
                    let action = updates.get("action").and_then(|a| a.as_str()).unwrap_or("");
                    match action {
                        "activate" => plan.activate(),
                        "complete" => plan.complete(),
                        "abandon" => plan.abandon(),
                        "add_milestone" => {
                            if let Some(desc) = updates.get("description").and_then(|d| d.as_str()) {
                                plan.add_milestone(desc.to_string());
                            }
                        }
                        "log_decision" => {
                            let decision = updates.get("decision").and_then(|d| d.as_str()).unwrap_or("");
                            let rationale = updates.get("rationale").and_then(|r| r.as_str()).unwrap_or("");
                            plan.log_decision(decision, rationale);
                        }
                        _ => return RpcResponse::error(id, INTERNAL_ERROR, format!("unknown action: {action}")),
                    }
                    match serde_json::to_value(&*plan) {
                        Ok(v) => RpcResponse::success(id, v),
                        Err(e) => RpcResponse::error(id, INTERNAL_ERROR, e.to_string()),
                    }
                }
                None => RpcResponse::error(id, INTERNAL_ERROR, "plan not found"),
            }
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
        let thread_db = crate::thread_db::ThreadDb::open(&dir.join("threads.db")).await?;
        Ok(AppState {
            server,
            tasks,
            skills: Arc::new(RwLock::new(harness_skills::SkillStore::new())),
            rules: Arc::new(RwLock::new(harness_rules::engine::RuleEngine::new())),
            events,
            gc_agent,
            plans: Arc::new(RwLock::new(std::collections::HashMap::new())),
            thread_db: Some(thread_db),
        })
    }

    #[tokio::test]
    async fn gc_adopt_response_includes_task_id() -> anyhow::Result<()> {
        use harness_core::{
            Artifact, ArtifactType, Draft, DraftId, DraftStatus, ProjectId, RemediationType,
            Signal, SignalType,
        };

        let dir = tempfile::tempdir()?;
        let state = make_test_state(dir.path()).await?;

        // Seed a pending draft in the store.
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

        assert!(resp.error.is_none(), "expected success, got error: {:?}", resp.error);
        let result = resp.result.ok_or_else(|| anyhow::anyhow!("missing result"))?;
        assert_eq!(result["adopted"], serde_json::json!(true), "adopted must be true");
        assert_eq!(result["task_id"], serde_json::json!(null), "task_id should be null when no agent is registered");
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
        let resp = handle_request(&state, req).await;

        assert!(resp.error.is_none(), "expected success, got error: {:?}", resp.error);
        let result = resp.result.ok_or_else(|| anyhow::anyhow!("missing result"))?;
        assert_eq!(result["adopted"], serde_json::json!(true), "adopted must be true");
        let task_id = result["task_id"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("task_id should be a string"))?;
        assert!(!task_id.is_empty(), "task_id should be non-empty");
        // Verify the task was actually created in the task store.
        let tid = crate::task_runner::TaskId(task_id.to_string());
        let task = state.tasks.get(&tid);
        assert!(task.is_some(), "task should exist in the task store");
        Ok(())
    }

    #[tokio::test]
    async fn rule_check_logs_no_violations_when_no_guards_loaded() -> anyhow::Result<()> {
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

        assert!(resp.error.is_none(), "expected success: {:?}", resp.error);
        let violations: Vec<serde_json::Value> = serde_json::from_value(
            resp.result.ok_or_else(|| anyhow::anyhow!("missing result"))?,
        )?;
        assert!(violations.is_empty(), "no guards loaded, violations must be empty");

        // No rule_check events should have been logged with no violations.
        let events = state.events.query(&harness_core::EventFilters {
            hook: Some("rule_check".to_string()),
            ..Default::default()
        })?;
        assert!(events.is_empty(), "no events should be logged when there are no violations");
        Ok(())
    }

    #[tokio::test]
    async fn metrics_query_counts_rule_violations_from_events() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let state = make_test_state(dir.path()).await?;

        // Manually seed rule_check violation events (as RuleCheck handler would).
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

        assert!(resp.error.is_none(), "expected success: {:?}", resp.error);
        let result = resp.result.ok_or_else(|| anyhow::anyhow!("missing result"))?;
        // Coverage must be below 100% because we have 5 Block violations.
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
            method: Method::ThreadStart { cwd: std::path::PathBuf::from("/test/project") },
        };
        let resp = handle_request(&state, req).await;

        assert!(resp.error.is_none(), "expected success, got error: {:?}", resp.error);
        let thread_id_str = resp.result.unwrap()["thread_id"]
            .as_str()
            .unwrap()
            .to_string();

        let db = state.thread_db.as_ref().unwrap();
        let thread = db.get(&thread_id_str).await?.expect("thread should be in DB");
        assert_eq!(thread.project_root, std::path::PathBuf::from("/test/project"));
        Ok(())
    }
}
