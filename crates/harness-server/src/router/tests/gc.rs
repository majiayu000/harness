use super::*;

struct MockAgent;

#[async_trait::async_trait]
impl harness_core::agent::CodeAgent for MockAgent {
    fn name(&self) -> &str {
        "mock"
    }
    fn capabilities(&self) -> Vec<harness_core::types::Capability> {
        vec![]
    }
    async fn execute(
        &self,
        _req: harness_core::agent::AgentRequest,
    ) -> harness_core::error::Result<harness_core::agent::AgentResponse> {
        Ok(harness_core::agent::AgentResponse {
            output: "LGTM".to_string(),
            stderr: String::new(),
            items: vec![],
            token_usage: harness_core::types::TokenUsage::default(),
            model: "mock".to_string(),
            exit_code: Some(0),
        })
    }
    async fn execute_stream(
        &self,
        _req: harness_core::agent::AgentRequest,
        _tx: tokio::sync::mpsc::Sender<harness_core::agent::StreamItem>,
    ) -> harness_core::error::Result<()> {
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
impl harness_core::agent::CodeAgent for NonLgtmAgent {
    fn name(&self) -> &str {
        "non-lgtm"
    }

    fn capabilities(&self) -> Vec<harness_core::types::Capability> {
        vec![]
    }

    async fn execute(
        &self,
        _req: harness_core::agent::AgentRequest,
    ) -> harness_core::error::Result<harness_core::agent::AgentResponse> {
        let call = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let output = if call == 0 {
            "Implemented\nPR_URL=https://github.com/example/repo/pull/123".to_string()
        } else {
            "Needs follow-up\nFIXED".to_string()
        };
        Ok(harness_core::agent::AgentResponse {
            output,
            stderr: String::new(),
            items: vec![],
            token_usage: harness_core::types::TokenUsage::default(),
            model: "mock".to_string(),
            exit_code: Some(0),
        })
    }

    async fn execute_stream(
        &self,
        _req: harness_core::agent::AgentRequest,
        tx: tokio::sync::mpsc::Sender<harness_core::agent::StreamItem>,
    ) -> harness_core::error::Result<()> {
        let call = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let output = if call == 0 {
            "Implemented\nPR_URL=https://github.com/example/repo/pull/123\n"
        } else {
            "Needs follow-up\nFIXED\n"
        };
        if tx
            .send(harness_core::agent::StreamItem::MessageDelta {
                text: output.to_string(),
            })
            .await
            .is_err()
        {
            return Ok(());
        }
        // Receiver may have dropped; ignore send error for Done sentinel.
        tx.send(harness_core::agent::StreamItem::Done).await.ok();
        Ok(())
    }
}

async fn wait_for_terminal_task(
    state: &AppState,
    task_id: &str,
) -> anyhow::Result<crate::task_runner::TaskState> {
    use tokio::time::{sleep, Duration};

    let tid = harness_core::types::TaskId(task_id.to_string());
    for _ in 0..120 {
        if let Some(task) = state.core.tasks.get(&tid) {
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
    let _lock = crate::test_helpers::HOME_LOCK.lock().await;
    let dir = crate::test_helpers::tempdir_in_home("harness-gc-test-")?;
    let mut config = HarnessConfig::default();
    config.gc.adopt_wait_secs = 0;
    config.gc.adopt_max_rounds = max_rounds;
    config.gc.adopt_turn_timeout_secs = 30;
    // Disable Jaccard loop detection so this test can verify max_rounds exhaustion.
    // NonLgtmAgent intentionally returns identical output every round.
    config.concurrency.loop_jaccard_threshold = 1.1;

    let mut registry = AgentRegistry::new("mock");
    registry.register("mock", Arc::new(NonLgtmAgent::new()));
    let state =
        super::make_test_state_with_config_and_registry(dir.path(), config, registry).await?;

    let draft_id = harness_core::types::DraftId::new();
    let signal = harness_core::types::Signal::new(
        harness_core::types::SignalType::RepeatedWarn,
        harness_core::types::ProjectId::new(),
        serde_json::json!("test signal"),
        harness_core::types::RemediationType::Guard,
    );
    let draft = harness_core::types::Draft {
        id: draft_id.clone(),
        status: harness_core::types::DraftStatus::Pending,
        signal,
        artifacts: vec![harness_core::types::Artifact {
            artifact_type: harness_core::types::ArtifactType::Guard,
            target_path: std::path::PathBuf::from("test-guard.sh"),
            content: "#!/bin/bash\necho ok".to_string(),
        }],
        rationale: "test".to_string(),
        validation: "test".to_string(),
        generated_at: chrono::Utc::now(),
        agent_model: "test".to_string(),
    };
    state.engines.gc_agent.draft_store().save(&draft)?;

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
async fn gc_adopt_response_includes_task_id() -> anyhow::Result<()> {
    use harness_core::types::{
        Artifact, ArtifactType, Draft, DraftId, DraftStatus, ProjectId, RemediationType, Signal,
        SignalType,
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
            target_path: std::path::PathBuf::from("test-guard.sh"),
            content: "#!/bin/bash\necho ok".to_string(),
        }],
        rationale: "test".to_string(),
        validation: "test".to_string(),
        generated_at: chrono::Utc::now(),
        agent_model: "test".to_string(),
    };
    state.engines.gc_agent.draft_store().save(&draft)?;

    let req = RpcRequest {
        jsonrpc: "2.0".to_string(),
        id: Some(serde_json::json!(1)),
        method: Method::GcAdopt { draft_id },
    };
    let resp = handle_request(&state, req)
        .await
        .expect("expected response for request with id");

    let err = resp
        .error
        .ok_or_else(|| anyhow::anyhow!("expected error when default agent is missing"))?;
    assert_eq!(
        err.code,
        harness_protocol::methods::INTERNAL_ERROR,
        "gc_adopt should fail fast when auto_pr is enabled but no default agent exists"
    );
    assert!(
        err.message
            .contains("auto_pr requires a registered default agent"),
        "unexpected error message: {}",
        err.message
    );
    Ok(())
}

#[tokio::test]
async fn gc_adopt_spawns_task_when_agent_registered() -> anyhow::Result<()> {
    use harness_core::types::{
        Artifact, ArtifactType, Draft, DraftId, DraftStatus, ProjectId, RemediationType, Signal,
        SignalType,
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
    let draft = Draft {
        id: draft_id.clone(),
        status: DraftStatus::Pending,
        signal,
        artifacts: vec![Artifact {
            artifact_type: ArtifactType::Guard,
            target_path: std::path::PathBuf::from("test-guard.sh"),
            content: "#!/bin/bash\necho ok".to_string(),
        }],
        rationale: "test".to_string(),
        validation: "test".to_string(),
        generated_at: chrono::Utc::now(),
        agent_model: "test".to_string(),
    };
    state.engines.gc_agent.draft_store().save(&draft)?;

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
    let tid = harness_core::types::TaskId(task_id.to_string());
    let task = state.core.tasks.get(&tid);
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
