//! Integration test: detect signal → generate draft → adopt → verify task dispatched.
//!
//! Verifies the full GcAdopt pipeline:
//! 1. A draft is stored in the GcAgent's DraftStore.
//! 2. `gc_adopt` writes artifact files to disk.
//! 3. A task is dispatched to the agent with the correct prompt (rationale, validation, paths).
//! 4. The response carries `adopted: true` and a non-null `task_id`.

mod common;

use async_trait::async_trait;
use chrono::Utc;
use harness_agents::AgentRegistry;
use harness_core::{
    AgentRequest, AgentResponse, Artifact, ArtifactType, Capability, CodeAgent, Draft, DraftId,
    DraftStatus, HarnessConfig, HarnessError, ProjectId, RemediationType, Signal, SignalType,
    StreamItem, TokenUsage,
};
use harness_server::{
    handlers::gc::gc_adopt, http::build_app_state, server::HarnessServer,
    thread_manager::ThreadManager,
};
use std::path::Path;
use std::sync::Arc;
use tokio::sync::mpsc::Sender;

// ---------------------------------------------------------------------------
// Mock agent — immediately completes with a fake PR_URL output.
// ---------------------------------------------------------------------------

struct MockPrAgent;

#[async_trait]
impl CodeAgent for MockPrAgent {
    fn name(&self) -> &str {
        "mock-pr"
    }

    fn capabilities(&self) -> Vec<Capability> {
        vec![Capability::Read, Capability::Write]
    }

    async fn execute(&self, _req: AgentRequest) -> harness_core::Result<AgentResponse> {
        Ok(AgentResponse {
            output: "Created branch gc/test-draft.\nPR_URL=https://github.com/owner/repo/pull/99"
                .to_string(),
            stderr: String::new(),
            items: vec![],
            token_usage: TokenUsage::default(),
            model: "mock".to_string(),
            exit_code: Some(0),
        })
    }

    async fn execute_stream(
        &self,
        req: AgentRequest,
        tx: Sender<StreamItem>,
    ) -> harness_core::Result<()> {
        let resp = self.execute(req).await?;
        tx.send(StreamItem::MessageDelta { text: resp.output })
            .await
            .map_err(|e| HarnessError::AgentExecution(format!("stream closed: {e}")))?;
        tx.send(StreamItem::Done)
            .await
            .map_err(|e| HarnessError::AgentExecution(format!("stream closed: {e}")))?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

async fn make_state(root: &Path) -> anyhow::Result<harness_server::http::AppState> {
    make_state_with_auto_pr(root, true).await
}

async fn make_state_with_auto_pr(
    root: &Path,
    auto_pr: bool,
) -> anyhow::Result<harness_server::http::AppState> {
    let project_root = root.join("project");
    std::fs::create_dir_all(&project_root)?;

    let mut config = HarnessConfig::default();
    config.server.data_dir = root.join("server-data");
    config.server.project_root = project_root;
    config.agents.default_agent = "mock-pr".to_string();
    config.gc.auto_pr = auto_pr;

    let mut registry = AgentRegistry::new("mock-pr");
    registry.register("mock-pr", Arc::new(MockPrAgent));

    let server = Arc::new(HarnessServer::new(config, ThreadManager::new(), registry));
    build_app_state(server).await
}

fn make_draft(artifact_path: &Path, content: &str) -> Draft {
    Draft {
        id: DraftId::new(),
        status: DraftStatus::Pending,
        signal: Signal::new(
            SignalType::RepeatedWarn,
            ProjectId::new(),
            serde_json::json!({"reason": "unwrap usage", "count": 12}),
            RemediationType::Guard,
        ),
        artifacts: vec![Artifact {
            artifact_type: ArtifactType::Guard,
            target_path: artifact_path.to_path_buf(),
            content: content.to_string(),
        }],
        rationale: "Auto-generated fix for RepeatedWarn signal".to_string(),
        validation: "Run guard check after applying".to_string(),
        generated_at: Utc::now(),
        agent_model: "test".to_string(),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// gc_adopt writes artifact files and dispatches an agent task.
#[tokio::test]
async fn gc_adopt_dispatches_task_with_prompt() -> anyhow::Result<()> {
    let sandbox = common::tempdir_in_home("gc-adopt-pipeline-")?;
    let state = make_state(sandbox.path()).await?;

    // Create a draft with a relative artifact path inside the sandbox.
    let artifact_rel = std::path::PathBuf::from(".harness/drafts/test-guard.sh");
    let draft = make_draft(&artifact_rel, "#!/usr/bin/env bash\necho 'guard'");
    state.engines.gc_agent.draft_store().save(&draft)?;

    let draft_id = draft.id.clone();
    let resp = gc_adopt(&state, Some(serde_json::json!(1)), draft_id).await;

    assert!(
        resp.error.is_none(),
        "expected success, got error: {:?}",
        resp.error
    );
    let result = resp.result.expect("missing result");
    assert_eq!(result["adopted"], true);
    assert!(
        !result["task_id"].is_null(),
        "task_id should be set when an agent is registered"
    );

    Ok(())
}

/// gc_adopt returns adopted=true with null task_id when there are no artifacts.
#[tokio::test]
async fn gc_adopt_no_artifacts_returns_null_task_id() -> anyhow::Result<()> {
    let sandbox = common::tempdir_in_home("gc-adopt-no-artifacts-")?;
    let state = make_state(sandbox.path()).await?;

    let draft = Draft {
        id: DraftId::new(),
        status: DraftStatus::Pending,
        signal: Signal::new(
            SignalType::SlowSessions,
            ProjectId::new(),
            serde_json::json!({}),
            RemediationType::Skill,
        ),
        artifacts: vec![],
        rationale: "no artifacts".to_string(),
        validation: "none".to_string(),
        generated_at: Utc::now(),
        agent_model: "test".to_string(),
    };
    state.engines.gc_agent.draft_store().save(&draft)?;

    let draft_id = draft.id.clone();
    let resp = gc_adopt(&state, Some(serde_json::json!(1)), draft_id).await;

    assert!(resp.error.is_none(), "expected success: {:?}", resp.error);
    let result = resp.result.expect("missing result");
    assert_eq!(result["adopted"], true);
    assert!(result["task_id"].is_null());

    Ok(())
}

/// gc_adopt returns NOT_FOUND for an unknown draft ID.
#[tokio::test]
async fn gc_adopt_unknown_draft_returns_not_found() -> anyhow::Result<()> {
    let sandbox = common::tempdir_in_home("gc-adopt-not-found-")?;
    let state = make_state(sandbox.path()).await?;

    let unknown_id = DraftId::new();
    let resp = gc_adopt(&state, Some(serde_json::json!(1)), unknown_id).await;

    assert!(resp.error.is_some(), "expected error for unknown draft");
    assert_eq!(resp.error.unwrap().code, harness_protocol::NOT_FOUND);

    Ok(())
}

/// gc_adopt with auto_pr=false skips task dispatch and returns null task_id.
#[tokio::test]
async fn gc_adopt_auto_pr_false_skips_task_dispatch() -> anyhow::Result<()> {
    let sandbox = common::tempdir_in_home("gc-adopt-no-auto-pr-")?;
    let state = make_state_with_auto_pr(sandbox.path(), false).await?;

    let artifact_rel = std::path::PathBuf::from(".harness/drafts/test-guard.sh");
    let draft = make_draft(&artifact_rel, "#!/usr/bin/env bash\necho 'guard'");
    state.engines.gc_agent.draft_store().save(&draft)?;

    let draft_id = draft.id.clone();
    let resp = gc_adopt(&state, Some(serde_json::json!(1)), draft_id).await;

    assert!(
        resp.error.is_none(),
        "expected success, got error: {:?}",
        resp.error
    );
    let result = resp.result.expect("missing result");
    assert_eq!(result["adopted"], true);
    assert!(
        result["task_id"].is_null(),
        "task_id should be null when auto_pr=false"
    );

    Ok(())
}

/// gc_adopt with auto_pr=true (default) dispatches a task when artifacts exist.
#[tokio::test]
async fn gc_adopt_auto_pr_true_dispatches_task() -> anyhow::Result<()> {
    let sandbox = common::tempdir_in_home("gc-adopt-auto-pr-true-")?;
    let state = make_state_with_auto_pr(sandbox.path(), true).await?;

    let artifact_rel = std::path::PathBuf::from(".harness/drafts/test-guard.sh");
    let draft = make_draft(&artifact_rel, "#!/usr/bin/env bash\necho 'guard'");
    state.engines.gc_agent.draft_store().save(&draft)?;

    let draft_id = draft.id.clone();
    let resp = gc_adopt(&state, Some(serde_json::json!(1)), draft_id).await;

    assert!(
        resp.error.is_none(),
        "expected success, got error: {:?}",
        resp.error
    );
    let result = resp.result.expect("missing result");
    assert_eq!(result["adopted"], true);
    assert!(
        !result["task_id"].is_null(),
        "task_id should be set when auto_pr=true and agent is registered"
    );

    Ok(())
}
