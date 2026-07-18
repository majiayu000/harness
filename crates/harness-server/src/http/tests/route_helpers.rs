use super::*;

pub(super) fn init_fake_git_repo(root: &std::path::Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(root.join(".git"))?;
    Ok(())
}

pub(super) fn init_worktree_git_repo(root: &std::path::Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(root)?;
    run_git(root, &["init"])?;
    run_git(root, &["config", "user.email", "test@harness.test"])?;
    run_git(root, &["config", "user.name", "Harness Test"])?;
    run_git(root, &["commit", "--allow-empty", "-m", "init"])?;
    run_git(root, &["branch", "-M", "main"])?;
    Ok(())
}

fn run_git(root: &std::path::Path, args: &[&str]) -> anyhow::Result<()> {
    let git_bin = std::env::var("HARNESS_GIT_BIN").unwrap_or_else(|_| "git".to_string());
    let mut command = std::process::Command::new(git_bin);
    for key in [
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_CONFIG",
        "GIT_CONFIG_PARAMETERS",
        "GIT_CONFIG_COUNT",
        "GIT_OBJECT_DIRECTORY",
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_IMPLICIT_WORK_TREE",
        "GIT_GRAFT_FILE",
        "GIT_INDEX_FILE",
        "GIT_NO_REPLACE_OBJECTS",
        "GIT_REPLACE_REF_BASE",
        "GIT_PREFIX",
        "GIT_SHALLOW_FILE",
        "GIT_COMMON_DIR",
    ] {
        command.env_remove(key);
    }
    let output = command.arg("-C").arg(root).args(args).output()?;
    if !output.status.success() {
        anyhow::bail!(
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

pub(super) async fn call_health(state: Arc<AppState>) -> anyhow::Result<HealthResponse> {
    use http_body_util::BodyExt;
    let app = Router::new()
        .route("/health", get(health_check))
        .with_state(state);
    let response = app
        .oneshot(Request::builder().uri("/health").body(Body::empty())?)
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.into_body().collect().await?.to_bytes();
    Ok(serde_json::from_slice(&body)?)
}

#[tokio::test]
async fn get_task_proof_returns_runtime_backed_terminal_task() -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let state = make_test_state_with_workflow_runtime(dir.path()).await?;
    let store = state
        .core
        .workflow_runtime_store
        .as_ref()
        .expect("workflow runtime store should be configured");
    let task_id = "runtime-proof-task";
    let pr_url = "https://github.com/owner/repo/pull/77";
    let workflow = harness_workflow::runtime::WorkflowInstance::new(
        harness_workflow::runtime::GITHUB_ISSUE_PR_DEFINITION_ID,
        1,
        "done",
        harness_workflow::runtime::WorkflowSubject::new("issue", "issue:1111"),
    )
    .with_id("runtime-proof-workflow")
    .with_data(serde_json::json!({
        "task_id": task_id,
        "task_ids": [task_id],
        "project_id": "/project-a",
        "repo": "owner/repo",
        "issue_number": 1111,
        "pr_number": 77,
        "pr_url": pr_url,
    }));
    store.upsert_instance(&workflow).await?;
    let event = store
        .append_event(
            &workflow.id,
            "PrReadyToMerge",
            "workflow-runtime-test",
            serde_json::json!({ "task_id": task_id, "pr_url": pr_url }),
        )
        .await?;
    let decision = harness_workflow::runtime::WorkflowDecision::new(
        &workflow.id,
        "awaiting_feedback",
        "mark_ready_to_merge",
        "ready_to_merge",
        "review, checks, and mergeability are ready",
    )
    .high_confidence();
    store
        .record_decision(
            &harness_workflow::runtime::WorkflowDecisionRecord::accepted(decision, Some(event.id)),
        )
        .await?;
    assert!(
        state
            .core
            .tasks
            .get_with_db_fallback(&task_runner::TaskId::from_str(task_id))
            .await?
            .is_none(),
        "runtime-backed task should not have a legacy task row"
    );

    let response = runtime_submission_app(state)
        .oneshot(
            Request::builder()
                .uri(format!(
                    "/api/workflows/runtime/submissions/{task_id}/proof"
                ))
                .body(Body::empty())?,
        )
        .await?;

    assert_eq!(response.status(), StatusCode::OK);
    let body = response_json(response).await?;
    assert_eq!(body["task_id"], task_id);
    assert_eq!(body["status"], "done");
    assert_eq!(body["pr_url"], pr_url);
    assert_eq!(body["ci_status"], "passed");
    assert_eq!(body["review_outcome"], "approved");
    assert_eq!(body["review_rounds"], 1);
    assert!(body["quality_signals"]
        .as_array()
        .expect("quality signals should be an array")
        .iter()
        .any(
            |signal| signal["name"] == "workflow_id" && signal["value"] == "runtime-proof-workflow"
        ));
    Ok(())
}

#[tokio::test]
async fn get_task_proof_rejects_nonterminal_runtime_task() -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let state = make_test_state_with_workflow_runtime(dir.path()).await?;
    let store = state
        .core
        .workflow_runtime_store
        .as_ref()
        .expect("workflow runtime store should be configured");
    let task_id = "runtime-proof-active";
    let workflow = harness_workflow::runtime::WorkflowInstance::new(
        harness_workflow::runtime::PROMPT_TASK_DEFINITION_ID,
        1,
        "implementing",
        harness_workflow::runtime::WorkflowSubject::new("prompt", "prompt:active"),
    )
    .with_id("runtime-proof-active-workflow")
    .with_data(serde_json::json!({
        "task_id": task_id,
        "project_id": "/project-a",
    }));
    store.upsert_instance(&workflow).await?;

    let response = runtime_submission_app(state)
        .oneshot(
            Request::builder()
                .uri(format!(
                    "/api/workflows/runtime/submissions/{task_id}/proof"
                ))
                .body(Body::empty())?,
        )
        .await?;

    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let body = response_json(response).await?;
    assert_eq!(
        body["error"],
        "runtime submission is not in a terminal state"
    );
    assert_eq!(body["status"], "implementing");
    Ok(())
}

pub(super) fn intake_app(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/api/intake", get(intake_status))
        .with_state(state)
}

pub(super) fn token_usage_app(state: Arc<AppState>) -> Router {
    Router::new()
        .route(
            "/api/token-usage",
            get(crate::handlers::token_usage::token_usage),
        )
        .with_state(state)
}

pub(super) fn workflow_runtime_app(state: Arc<AppState>) -> Router {
    Router::new()
        .route(
            "/api/workflows/runtime/tree",
            get(get_workflow_runtime_tree),
        )
        .with_state(state)
}

pub(super) fn runtime_submission_app(state: Arc<AppState>) -> Router {
    Router::new()
        .route(
            "/api/workflows/runtime/submissions",
            get(task_query_routes::list_runtime_submissions)
                .post(task_routes::create_runtime_submission),
        )
        .route(
            "/api/workflows/runtime/submissions/{id}",
            get(task_query_routes::get_runtime_submission),
        )
        .route(
            "/api/workflows/runtime/submissions/{id}/artifacts",
            get(runtime_submission_routes::get_artifacts),
        )
        .route(
            "/api/workflows/runtime/submissions/{id}/prompts",
            get(runtime_submission_routes::get_prompts),
        )
        .route(
            "/api/workflows/runtime/submissions/{id}/proof",
            get(task_query_routes::get_runtime_submission_proof),
        )
        .with_state(state)
}

pub(super) fn webhook_app(state: Arc<AppState>) -> Router {
    let body_limit = state.core.server.config.server.max_webhook_body_bytes;
    Router::new()
        .route(
            "/webhook",
            post(github_webhook).layer(DefaultBodyLimit::max(body_limit)),
        )
        .route(
            "/webhook/feishu",
            post(crate::intake::feishu::feishu_webhook).layer(DefaultBodyLimit::max(body_limit)),
        )
        .with_state(state)
}

pub(super) fn make_feishu_config(
    verification_token: Option<&str>,
) -> harness_core::config::intake::FeishuIntakeConfig {
    harness_core::config::intake::FeishuIntakeConfig {
        enabled: true,
        app_id: None,
        app_secret: None,
        verification_token: verification_token.map(ToString::to_string),
        trigger_keyword: "harness".to_string(),
        default_repo: None,
    }
}

pub(super) async fn make_test_state_with_feishu(
    dir: &std::path::Path,
    verification_token: Option<&str>,
) -> anyhow::Result<Arc<AppState>> {
    let mut config = harness_core::config::HarnessConfig::default();
    config.intake.feishu = Some(make_feishu_config(verification_token));
    make_test_state_with(
        dir,
        config,
        harness_agents::registry::AgentRegistry::new("test"),
    )
    .await
}

pub(super) fn feishu_challenge_payload(token: Option<&str>) -> serde_json::Value {
    match token {
        Some(token) => serde_json::json!({ "challenge": "challenge-123", "token": token }),
        None => serde_json::json!({ "challenge": "challenge-123" }),
    }
}

pub(super) fn feishu_event_payload(token: Option<&str>) -> serde_json::Value {
    let content = serde_json::to_string(&serde_json::json!({ "text": "harness fix login bug" }))
        .expect("serialize feishu content");
    match token {
        Some(token) => serde_json::json!({
            "header": { "event_type": "im.message.receive_v1", "token": token },
            "event": {
                "message": {
                    "message_id": "msg-001",
                    "chat_id": "chat-001",
                    "message_type": "text",
                    "content": content
                }
            }
        }),
        None => serde_json::json!({
            "header": { "event_type": "im.message.receive_v1" },
            "event": {
                "message": {
                    "message_id": "msg-001",
                    "chat_id": "chat-001",
                    "message_type": "text",
                    "content": content
                }
            }
        }),
    }
}

pub(super) async fn response_json(
    response: axum::response::Response,
) -> anyhow::Result<serde_json::Value> {
    use http_body_util::BodyExt;
    let body = response.into_body().collect().await?.to_bytes();
    Ok(serde_json::from_slice(&body)?)
}

pub(super) async fn assert_runtime_issue_submission(
    state: &Arc<AppState>,
    project_root: &std::path::Path,
    repo: Option<&str>,
    issue_number: u64,
    task_id: &str,
) -> anyhow::Result<String> {
    let task_id = task_runner::TaskId::from_str(task_id);
    assert!(
        state
            .core
            .tasks
            .get_with_db_fallback(&task_id)
            .await?
            .is_none(),
        "workflow runtime issue submissions must not register legacy task rows"
    );
    let store = state
        .core
        .workflow_runtime_store
        .as_ref()
        .expect("workflow runtime store should be configured");
    let canonical_project_root = project_root.canonicalize()?;
    let workflow_id = harness_workflow::issue_lifecycle::workflow_id(
        &canonical_project_root.to_string_lossy(),
        repo,
        issue_number,
    );
    let instance = store
        .get_instance(&workflow_id)
        .await?
        .expect("runtime workflow should be persisted");
    assert_eq!(instance.state, "planning");
    assert_eq!(instance.data["task_id"], task_id.0);
    assert_eq!(instance.data["execution_path"], "workflow_runtime");
    let commands = store.commands_for(&workflow_id).await?;
    assert_eq!(commands.len(), 1);
    assert_eq!(commands[0].status, "pending");
    assert_eq!(commands[0].command.activity_name(), Some("plan_issue"));

    let get_response = runtime_submission_app(state.clone())
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!("/api/workflows/runtime/submissions/{}", task_id.0))
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(get_response.status(), StatusCode::OK);
    let runtime_task = response_json(get_response).await?;
    assert_eq!(runtime_task["task_id"], task_id.0);
    assert_eq!(runtime_task["status"], "planning");
    assert_eq!(runtime_task["execution_path"], "workflow_runtime");
    assert_eq!(runtime_task["workflow_id"], workflow_id);
    Ok(workflow_id)
}

pub(super) async fn seed_bound_runtime_pr_workflow(
    state: &Arc<AppState>,
    project_root: &std::path::Path,
    repo: &str,
    issue_number: u64,
    pr_number: u64,
) -> anyhow::Result<(String, String)> {
    let store = state
        .core
        .workflow_runtime_store
        .as_ref()
        .expect("workflow runtime store should be configured");
    let canonical_project_root = project_root.canonicalize()?;
    let project_id = canonical_project_root.to_string_lossy().into_owned();
    let workflow_id =
        harness_workflow::issue_lifecycle::workflow_id(&project_id, Some(repo), issue_number);
    let task_id = format!("runtime-issue-{issue_number}");
    let instance = harness_workflow::runtime::WorkflowInstance::new(
        harness_workflow::runtime::GITHUB_ISSUE_PR_DEFINITION_ID,
        1,
        "pr_open",
        harness_workflow::runtime::WorkflowSubject::new("issue", format!("issue:{issue_number}")),
    )
    .with_id(workflow_id.clone())
    .with_data(serde_json::json!({
        "project_id": project_id,
        "repo": repo,
        "issue_number": issue_number,
        "task_id": task_id.clone(),
        "task_ids": [task_id.clone()],
        "pr_number": pr_number,
        "pr_url": format!("https://github.com/{repo}/pull/{pr_number}"),
        "execution_path": "workflow_runtime"
    }));
    store.upsert_instance(&instance).await?;
    Ok((workflow_id, task_id))
}

pub(super) async fn assert_runtime_local_review_requested(
    state: &Arc<AppState>,
    workflow_id: &str,
    task_id: &str,
) -> anyhow::Result<()> {
    let store = state
        .core
        .workflow_runtime_store
        .as_ref()
        .expect("workflow runtime store should be configured");
    let instance = store
        .get_instance(workflow_id)
        .await?
        .expect("runtime workflow should exist");
    assert_eq!(instance.state, "local_review_gate");
    assert_eq!(instance.data["task_id"], task_id);
    let commands = store.commands_for(workflow_id).await?;
    assert_eq!(commands.len(), 1);
    assert_eq!(commands[0].status, "pending");
    assert_eq!(
        commands[0].command.activity_name(),
        Some(harness_workflow::runtime::LOCAL_REVIEW_ACTIVITY)
    );
    assert!(
        state
            .core
            .tasks
            .get_with_db_fallback(&task_runner::TaskId::from_str(task_id))
            .await?
            .is_none(),
        "workflow runtime PR feedback must not register legacy task rows"
    );
    Ok(())
}

pub(super) async fn assert_runtime_prompt_submission(
    state: &Arc<AppState>,
    project_root: &std::path::Path,
    task_id: &str,
) -> anyhow::Result<String> {
    assert!(
        state
            .core
            .tasks
            .get_with_db_fallback(&task_runner::TaskId::from_str(task_id))
            .await?
            .is_none(),
        "workflow runtime prompt submissions must not register legacy task rows"
    );
    let store = state
        .core
        .workflow_runtime_store
        .as_ref()
        .expect("workflow runtime store should be configured");
    let instance = store
        .get_instance_by_task_id(task_id)
        .await?
        .expect("runtime prompt workflow should be persisted");
    assert_eq!(instance.definition_id, "prompt_task");
    assert_eq!(instance.state, "implementing");
    assert_eq!(instance.data["task_id"], task_id);
    assert_eq!(instance.data["execution_path"], "workflow_runtime");
    assert_eq!(
        instance.data["project_id"],
        project_root.canonicalize()?.to_string_lossy().as_ref()
    );
    let commands = store.commands_for(&instance.id).await?;
    assert_eq!(commands.len(), 1);
    assert_eq!(commands[0].status, "pending");
    assert_eq!(
        commands[0].command.activity_name(),
        Some("implement_prompt")
    );
    Ok(instance.id)
}

pub(super) fn webhook_signature(secret: &str, payload: &[u8]) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("valid hmac key");
    mac.update(payload);
    let digest = mac.finalize().into_bytes();
    let digest_hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    format!("sha256={digest_hex}")
}

#[derive(serde::Deserialize, Debug)]
pub(super) struct HealthResponse {
    pub(super) status: String,
    pub(super) tasks: u64,
    pub(super) persistence: PersistenceBlock,
    pub(super) runtime_logs: RuntimeLogsBlock,
    pub(super) runtime: serde_json::Value,
    pub(super) postgres_catalog: serde_json::Value,
    pub(super) isolation: serde_json::Value,
}

#[derive(serde::Deserialize, Debug)]
pub(super) struct PersistenceBlock {
    pub(super) degraded_subsystems: Vec<String>,
    pub(super) runtime_state_dirty: bool,
    pub(super) startup: StartupBlock,
}

#[derive(serde::Deserialize, Debug)]
pub(super) struct StartupBlock {
    pub(super) stores: Vec<StoreHealth>,
}

#[derive(serde::Deserialize, Debug)]
pub(super) struct StoreHealth {
    pub(super) name: String,
    pub(super) critical: bool,
    pub(super) ready: bool,
    pub(super) error: Option<String>,
}

#[derive(serde::Deserialize, Debug)]
pub(super) struct RuntimeLogsBlock {
    pub(super) state: String,
    pub(super) path_hint: Option<String>,
    pub(super) retention_days: u32,
    pub(super) retention_max_files: usize,
}
