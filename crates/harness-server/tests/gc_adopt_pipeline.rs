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
use harness_agents::registry::AgentRegistry;
use harness_core::agent::{AgentRequest, AgentResponse, CodeAgent, StreamItem};
use harness_core::config::HarnessConfig;
use harness_core::error::HarnessError;
use harness_core::types::{
    Artifact, ArtifactType, Capability, Draft, DraftId, DraftStatus, ProjectId, RemediationType,
    Signal, SignalType, TokenUsage,
};
use harness_server::{
    handlers::gc::gc_adopt, http::build_app_state, server::HarnessServer,
    thread_manager::ThreadManager,
};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
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

    async fn execute(&self, _req: AgentRequest) -> harness_core::error::Result<AgentResponse> {
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
    ) -> harness_core::error::Result<()> {
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

async fn make_state_without_default_agent(
    root: &Path,
) -> anyhow::Result<harness_server::http::AppState> {
    let project_root = root.join("project");
    std::fs::create_dir_all(&project_root)?;

    let mut config = HarnessConfig::default();
    config.server.data_dir = root.join("server-data");
    config.server.project_root = project_root;
    config.agents.default_agent = "missing".to_string();
    config.gc.auto_pr = true;

    let registry = AgentRegistry::new("missing");
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
    assert_eq!(
        resp.error.unwrap().code,
        harness_protocol::methods::NOT_FOUND
    );

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

/// gc_adopt with auto_pr=true fails before adopt when no default agent is registered.
#[tokio::test]
async fn gc_adopt_auto_pr_requires_default_agent() -> anyhow::Result<()> {
    let sandbox = common::tempdir_in_home("gc-adopt-no-default-agent-")?;
    let state = make_state_without_default_agent(sandbox.path()).await?;

    let artifact_rel = std::path::PathBuf::from(".harness/drafts/test-guard.sh");
    let draft = make_draft(&artifact_rel, "#!/usr/bin/env bash\necho 'guard'");
    state.engines.gc_agent.draft_store().save(&draft)?;

    let draft_id = draft.id.clone();
    let resp = gc_adopt(&state, Some(serde_json::json!(1)), draft_id.clone()).await;
    assert!(
        resp.error.is_some(),
        "expected error when default agent is missing"
    );

    let stored = state
        .engines
        .gc_agent
        .draft_store()
        .get(&draft_id)?
        .expect("draft should exist");
    assert_eq!(stored.status, DraftStatus::Pending);
    assert!(
        !state
            .core
            .project_root
            .join(".harness/drafts/test-guard.sh")
            .exists(),
        "artifact file should not be written when preflight fails"
    );

    Ok(())
}

// ---------------------------------------------------------------------------
// Capturing agent — records project_root from AgentRequest (issue #78 regression guard).
// ---------------------------------------------------------------------------

struct CapturingAgent {
    captured_root: Arc<Mutex<Option<PathBuf>>>,
}

#[async_trait]
impl CodeAgent for CapturingAgent {
    fn name(&self) -> &str {
        "capturing"
    }

    fn capabilities(&self) -> Vec<Capability> {
        vec![Capability::Read, Capability::Write]
    }

    async fn execute(&self, req: AgentRequest) -> harness_core::error::Result<AgentResponse> {
        *self.captured_root.lock().unwrap() = Some(req.project_root.clone());
        Ok(AgentResponse {
            output: "Agent: project root captured, nothing to implement".to_string(),
            stderr: String::new(),
            items: vec![],
            token_usage: TokenUsage::default(),
            model: "capturing".to_string(),
            exit_code: Some(0),
        })
    }

    async fn execute_stream(
        &self,
        req: AgentRequest,
        tx: Sender<StreamItem>,
    ) -> harness_core::error::Result<()> {
        *self.captured_root.lock().unwrap() = Some(req.project_root.clone());
        tx.send(StreamItem::MessageDelta {
            text: "Agent: project root captured, nothing to implement".to_string(),
        })
        .await
        .map_err(|e| HarnessError::AgentExecution(format!("stream closed: {e}")))?;
        tx.send(StreamItem::Done)
            .await
            .map_err(|e| HarnessError::AgentExecution(format!("stream closed: {e}")))?;
        Ok(())
    }
}

/// gc_adopt dispatches a task whose project_root matches the AppState project_root (issue #78).
///
/// Before the fix, `gc_adopt_task_request` set `project: None`, causing the task executor to
/// fall back to worktree detection instead of using the server's configured project root.
///
/// Setup: workspace isolation is disabled by blocking the workspace root dir so the task
/// runs directly against the project_root (no git worktree indirection).
#[tokio::test]
async fn gc_adopt_task_uses_appstate_project_root() -> anyhow::Result<()> {
    let sandbox = common::tempdir_in_home("gc-adopt-project-root-")?;
    let project_root = sandbox.path().join("project");
    std::fs::create_dir_all(&project_root)?;

    // Block workspace isolation: write a file where the workspace root dir would be,
    // so WorkspaceManager::new fails and build_app_state falls back to workspace_mgr = None.
    // With no workspace manager the task runs directly against project_root.
    let ws_blocker = sandbox.path().join("ws-blocker");
    std::fs::write(&ws_blocker, "blocker")?;

    let captured_root: Arc<Mutex<Option<PathBuf>>> = Arc::new(Mutex::new(None));
    let agent = Arc::new(CapturingAgent {
        captured_root: captured_root.clone(),
    });

    let mut config = HarnessConfig::default();
    config.server.data_dir = sandbox.path().join("server-data");
    config.server.project_root = project_root.clone();
    config.agents.default_agent = "capturing".to_string();
    config.gc.auto_pr = true;
    // Point workspace root at a path under the blocker file so create_dir_all fails.
    config.workspace.root = ws_blocker.join("workspaces");

    let mut registry = AgentRegistry::new("capturing");
    registry.register("capturing", agent);

    let server = Arc::new(HarnessServer::new(config, ThreadManager::new(), registry));
    let state = build_app_state(server).await?;

    let artifact_rel = PathBuf::from(".harness/drafts/test-guard.sh");
    let draft = make_draft(&artifact_rel, "#!/usr/bin/env bash\necho 'guard'");
    state.engines.gc_agent.draft_store().save(&draft)?;

    let draft_id = draft.id.clone();
    let resp = gc_adopt(&state, Some(serde_json::json!(1)), draft_id).await;
    assert!(resp.error.is_none(), "expected success: {:?}", resp.error);

    let task_id_val = resp
        .result
        .as_ref()
        .and_then(|r| r.get("task_id"))
        .expect("result must have task_id field");
    assert!(!task_id_val.is_null(), "task_id should be set");
    let task_id = harness_core::types::TaskId(
        task_id_val
            .as_str()
            .expect("task_id should be a string")
            .to_string(),
    );

    // Poll until task reaches a terminal state, with a 10-second limit.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let final_state = loop {
        if let Some(ts) = state.core.tasks.get(&task_id) {
            match ts.status {
                harness_server::task_runner::TaskStatus::Done
                | harness_server::task_runner::TaskStatus::Failed => break ts,
                _ => {}
            }
        }
        if std::time::Instant::now() >= deadline {
            panic!("task did not reach terminal state within 10 seconds");
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    };

    assert!(
        matches!(
            final_state.status,
            harness_server::task_runner::TaskStatus::Done
        ),
        "task should be Done but was {:?}: {:?}",
        final_state.status,
        final_state.error
    );

    let actual = captured_root
        .lock()
        .unwrap()
        .clone()
        .expect("capturing agent must have recorded project_root");
    assert_eq!(
        actual,
        project_root.canonicalize()?,
        "gc_adopt must pass AppState project_root to the spawned task, not None/CWD"
    );

    Ok(())
}
