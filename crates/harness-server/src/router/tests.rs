use super::*;
use crate::test_helpers::{make_test_state, make_test_state_with_registry};
use harness_agents::registry::AgentRegistry;
use harness_protocol::{methods::Method, methods::RpcRequest, methods::VALIDATION_ERROR};
use std::sync::Arc;

#[path = "tests/exec_plan.rs"]
mod exec_plan;

#[path = "tests/observability.rs"]
mod observability;

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
    state.notifications.initialized = Arc::new(std::sync::atomic::AtomicBool::new(false));

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
async fn gc_adopt_returns_runtime_submission_without_registered_agent() -> anyhow::Result<()> {
    use harness_core::types::{
        Artifact, ArtifactType, Draft, DraftId, DraftStatus, ProjectId, RemediationType, Signal,
        SignalType,
    };

    let dir = tempfile::tempdir()?;
    std::fs::create_dir_all(dir.path().join(".git"))?;
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

    assert!(resp.error.is_none(), "unexpected error: {:?}", resp.error);
    let result = resp
        .result
        .ok_or_else(|| anyhow::anyhow!("missing result"))?;
    let submission_id = result["submission_id"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("submission_id should be a string"))?;
    let workflow_id = result["workflow_id"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("workflow_id should be a string"))?;
    let store = state
        .core
        .workflow_runtime_store
        .as_ref()
        .expect("workflow runtime store should be configured");
    let workflow = store
        .get_instance(workflow_id)
        .await?
        .expect("GC adoption should persist a runtime workflow");
    assert_eq!(workflow.data["submission_id"], submission_id);
    assert!(state
        .core
        .tasks
        .get(&harness_core::types::TaskId(submission_id.to_string()))
        .is_none());
    Ok(())
}

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

#[tokio::test]
async fn gc_adopt_spawns_task_when_agent_registered() -> anyhow::Result<()> {
    use harness_core::types::{
        Artifact, ArtifactType, Draft, DraftId, DraftStatus, ProjectId, RemediationType, Signal,
        SignalType,
    };

    let dir = tempfile::tempdir()?;
    std::fs::create_dir_all(dir.path().join(".git"))?;
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
    let submission_id = result["submission_id"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("submission_id should be a string"))?;
    let workflow_id = result["workflow_id"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("workflow_id should be a string"))?;
    assert!(!submission_id.is_empty());
    let store = state
        .core
        .workflow_runtime_store
        .as_ref()
        .expect("workflow runtime store should be configured");
    let workflow = store
        .get_instance(workflow_id)
        .await?
        .expect("GC adoption should persist a runtime workflow");
    assert_eq!(workflow.data["submission_id"], submission_id);
    assert!(state
        .core
        .tasks
        .get(&harness_core::types::TaskId(submission_id.to_string()))
        .is_none());
    Ok(())
}
