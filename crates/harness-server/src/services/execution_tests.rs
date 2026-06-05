use super::*;
use harness_core::{
    agent::{AgentRequest, AgentResponse, StreamItem},
    types::{Capability, TokenUsage},
};
use std::sync::atomic::{AtomicBool, Ordering};

/// Minimal mock that always succeeds without touching any infrastructure.
struct AlwaysSucceedExecutionService {
    called: Arc<AtomicBool>,
}

impl AlwaysSucceedExecutionService {
    fn new() -> (Arc<Self>, Arc<AtomicBool>) {
        let called = Arc::new(AtomicBool::new(false));
        (
            Arc::new(Self {
                called: called.clone(),
            }),
            called,
        )
    }
}

struct NoopAgent;

#[async_trait]
impl CodeAgent for NoopAgent {
    fn name(&self) -> &str {
        "test"
    }

    fn capabilities(&self) -> Vec<Capability> {
        Vec::new()
    }

    async fn execute(&self, _req: AgentRequest) -> harness_core::error::Result<AgentResponse> {
        Ok(noop_agent_response())
    }

    async fn execute_stream(
        &self,
        _req: AgentRequest,
        _tx: tokio::sync::mpsc::Sender<StreamItem>,
    ) -> harness_core::error::Result<()> {
        Ok(())
    }
}

fn noop_agent_response() -> AgentResponse {
    AgentResponse {
        output: String::new(),
        stderr: String::new(),
        items: Vec::new(),
        token_usage: TokenUsage {
            input_tokens: 0,
            output_tokens: 0,
            total_tokens: 0,
            cost_usd: 0.0,
        },
        model: "test".to_string(),
        exit_code: Some(0),
    }
}

#[async_trait]
impl ExecutionService for AlwaysSucceedExecutionService {
    async fn enqueue(&self, _req: CreateTaskRequest) -> Result<TaskId, EnqueueTaskError> {
        self.called.store(true, Ordering::SeqCst);
        Ok(harness_core::types::TaskId("mock-task".to_string()))
    }

    async fn enqueue_in_domain(
        &self,
        _req: CreateTaskRequest,
        _queue_domain: QueueDomain,
    ) -> Result<TaskId, EnqueueTaskError> {
        self.called.store(true, Ordering::SeqCst);
        Ok(harness_core::types::TaskId("mock-task".to_string()))
    }

    async fn enqueue_background(
        &self,
        _req: CreateTaskRequest,
    ) -> Result<TaskId, EnqueueTaskError> {
        self.called.store(true, Ordering::SeqCst);
        Ok(harness_core::types::TaskId("mock-bg-task".to_string()))
    }

    async fn enqueue_background_with_options(
        &self,
        _req: CreateTaskRequest,
        _options: EnqueueBackgroundOptions,
    ) -> Result<TaskId, EnqueueTaskError> {
        self.called.store(true, Ordering::SeqCst);
        Ok(harness_core::types::TaskId("mock-bg-task".to_string()))
    }
}

#[tokio::test]
async fn mock_execution_service_enqueue() {
    let (svc, called) = AlwaysSucceedExecutionService::new();
    let req = CreateTaskRequest {
        prompt: Some("do something".to_string()),
        ..Default::default()
    };
    let task_id = svc.enqueue(req).await.unwrap();
    assert_eq!(task_id.0, "mock-task");
    assert!(called.load(Ordering::SeqCst));
}

#[tokio::test]
async fn mock_execution_service_enqueue_background() {
    let (svc, called) = AlwaysSucceedExecutionService::new();
    let req = CreateTaskRequest {
        prompt: Some("do something in bg".to_string()),
        ..Default::default()
    };
    let task_id = svc.enqueue_background(req).await.unwrap();
    assert_eq!(task_id.0, "mock-bg-task");
    assert!(called.load(Ordering::SeqCst));
}

#[tokio::test]
async fn enqueue_task_error_display() {
    let bad = EnqueueTaskError::BadRequest("missing prompt".to_string());
    let internal = EnqueueTaskError::Internal("db failed".to_string());
    assert!(bad.to_string().contains("bad request"));
    assert!(internal.to_string().contains("internal error"));
}

#[tokio::test]
async fn resolve_project_passes_through_none() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let store = TaskStore::open(&dir.path().join("t.db")).await?;
    let registry = ProjectRegistry::open(&dir.path().join("p.db")).await?;
    let svc = make_minimal_svc(store, Some(registry)).await;
    let result = svc.resolve_project(None).await?;
    assert!(result.0.is_none());
    assert!(result.1.is_none());
    Ok(())
}

#[tokio::test]
async fn resolve_project_passes_through_existing_dir() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let store = TaskStore::open(&dir.path().join("t.db")).await?;
    let registry = ProjectRegistry::open(&dir.path().join("p.db")).await?;
    let svc = make_minimal_svc(store, Some(registry)).await;

    let path = dir.path().to_path_buf();
    let canonical_path = path.canonicalize()?;
    let result = svc.resolve_project(Some(path.clone())).await?;
    assert_eq!(result.0, Some(canonical_path));
    assert!(result.1.is_none());
    Ok(())
}

#[tokio::test]
async fn resolve_project_unknown_id_returns_bad_request() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let store = TaskStore::open(&dir.path().join("t.db")).await?;
    let registry = ProjectRegistry::open(&dir.path().join("p.db")).await?;
    let svc = make_minimal_svc(store, Some(registry)).await;

    let result = svc
        .resolve_project(Some(PathBuf::from("nonexistent-id")))
        .await;
    assert!(matches!(result, Err(EnqueueTaskError::BadRequest(_))));
    Ok(())
}

#[tokio::test]
async fn validate_request_rejects_empty() {
    let req = CreateTaskRequest::default();
    assert!(matches!(
        DefaultExecutionService::validate_request(&req),
        Err(EnqueueTaskError::BadRequest(_))
    ));
}

#[tokio::test]
async fn validate_request_accepts_prompt() {
    let req = CreateTaskRequest {
        prompt: Some("hello".to_string()),
        ..Default::default()
    };
    assert!(DefaultExecutionService::validate_request(&req).is_ok());
}

#[test]
fn workflow_feedback_pr_request_bypasses_terminal_pr_duplicate_reuse() {
    let feedback_req = CreateTaskRequest {
        pr: Some(123),
        source: Some(WORKFLOW_FEEDBACK_SOURCE.to_string()),
        ..Default::default()
    };
    assert!(is_workflow_feedback_request(&feedback_req));
    assert!(!allows_terminal_pr_duplicate_reuse(&feedback_req));

    let regular_pr_req = CreateTaskRequest {
        pr: Some(123),
        ..Default::default()
    };
    assert!(!is_workflow_feedback_request(&regular_pr_req));
    assert!(allows_terminal_pr_duplicate_reuse(&regular_pr_req));
}

#[test]
fn runtime_prompt_duplicate_allows_blocked_resubmission() {
    let blocked = harness_workflow::runtime::WorkflowInstance::new(
        harness_workflow::runtime::PROMPT_TASK_DEFINITION_ID,
        1,
        "blocked",
        harness_workflow::runtime::WorkflowSubject::new("prompt", "manual:prompt:retry"),
    );
    let implementing = harness_workflow::runtime::WorkflowInstance::new(
        harness_workflow::runtime::PROMPT_TASK_DEFINITION_ID,
        1,
        "implementing",
        harness_workflow::runtime::WorkflowSubject::new("prompt", "manual:prompt:retry"),
    );

    assert!(runtime_prompt_duplicate_allows_resubmission(&blocked));
    assert!(!runtime_prompt_duplicate_allows_resubmission(&implementing));
}

#[tokio::test]
async fn check_allowed_roots_blocks_outside_root() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let store = TaskStore::open(&dir.path().join("t.db")).await?;
    let allowed = vec![PathBuf::from("/allowed/base")];
    let svc = make_svc_with_allowed_roots(store, allowed).await;
    let outside = PathBuf::from("/not/allowed");
    assert!(matches!(
        svc.check_allowed_roots(&outside),
        Err(EnqueueTaskError::BadRequest(_))
    ));
    Ok(())
}

#[tokio::test]
async fn check_allowed_roots_permits_inside_root() -> anyhow::Result<()> {
    let base_dir = tempfile::tempdir()?;
    let project_dir = base_dir.path().join("project");
    std::fs::create_dir_all(&project_dir)?;
    let dir = tempfile::tempdir()?;
    let store = TaskStore::open(&dir.path().join("t.db")).await?;
    let allowed = vec![base_dir.path().to_path_buf()];
    let svc = make_svc_with_allowed_roots(store, allowed).await;
    let canonical_project = project_dir.canonicalize()?;
    assert!(svc.check_allowed_roots(&canonical_project).is_ok());
    Ok(())
}

#[tokio::test]
async fn check_allowed_roots_empty_list_permits_all() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let store = TaskStore::open(&dir.path().join("t.db")).await?;
    let svc = make_svc_with_allowed_roots(store, vec![]).await;
    let any = PathBuf::from("/anywhere/repo");
    assert!(svc.check_allowed_roots(&any).is_ok());
    Ok(())
}

#[tokio::test]
async fn enqueue_background_issue_submission_requires_workflow_runtime_store() -> anyhow::Result<()>
{
    let dir = tempfile::tempdir()?;
    let project_root = dir.path().join("project");
    std::fs::create_dir(&project_root)?;
    let store = TaskStore::open(&dir.path().join("t.db")).await?;
    let task_store = store.clone();
    let svc = make_svc_with_agent_without_workflow_runtime(store).await;
    let req = CreateTaskRequest {
        issue: Some(42),
        repo: Some("owner/repo".to_string()),
        project: Some(project_root.clone()),
        ..Default::default()
    };

    let error = svc
        .enqueue_background(req)
        .await
        .expect_err("issue submission should fail closed without workflow runtime");

    assert!(
        matches!(error, EnqueueTaskError::Internal(ref message) if message.contains("workflow runtime store is required for GitHub issue submissions")),
        "unexpected error: {error}"
    );
    assert!(task_store.list_all().is_empty());
    Ok(())
}

#[tokio::test]
async fn enqueue_background_issue_submission_uses_workflow_runtime_without_task_runner(
) -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let project_root = dir.path().join("project");
    std::fs::create_dir(&project_root)?;
    let store = TaskStore::open(&dir.path().join("t.db")).await?;
    let task_store = store.clone();
    let database_url = crate::test_helpers::test_database_url()?;
    let runtime_store = Arc::new(
        harness_workflow::runtime::WorkflowRuntimeStore::open_with_database_url(
            &dir.path().join("workflow_runtime"),
            Some(&database_url),
        )
        .await?,
    );
    let svc = make_svc_with_workflow_runtime(store, runtime_store.clone()).await;
    let req = CreateTaskRequest {
        prompt: Some("preserve caller guidance".to_string()),
        issue: Some(42),
        repo: Some("owner/repo".to_string()),
        project: Some(project_root.clone()),
        labels: vec!["bug".to_string()],
        ..Default::default()
    };

    let task_id = svc.enqueue_background(req).await?;
    assert!(
        task_store.get_with_db_fallback(&task_id).await?.is_none(),
        "workflow runtime issue submissions must not register legacy task rows"
    );

    let canonical_project_root = project_root.canonicalize()?;
    let workflow_id = harness_workflow::issue_lifecycle::workflow_id(
        &canonical_project_root.to_string_lossy(),
        Some("owner/repo"),
        42,
    );
    let instance = runtime_store
        .get_instance(&workflow_id)
        .await?
        .expect("runtime workflow should be recorded");
    assert_eq!(instance.state, "implementing");
    assert_eq!(instance.data["task_id"], task_id.0);
    assert_eq!(
        instance.data["additional_prompt"],
        "preserve caller guidance"
    );
    assert_eq!(instance.data["execution_path"], "workflow_runtime");
    let commands = runtime_store.commands_for(&workflow_id).await?;
    assert_eq!(commands.len(), 1);
    assert_eq!(commands[0].status, "pending");
    assert_eq!(commands[0].command.activity_name(), Some("implement_issue"));
    assert_eq!(
        commands[0].command.command["additional_prompt"],
        "preserve caller guidance"
    );
    assert_eq!(runtime_store.pending_commands(10).await?.len(), 1);
    Ok(())
}

#[tokio::test]
async fn enqueue_background_prompt_submission_uses_workflow_runtime_without_task_runner(
) -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let project_root = dir.path().join("project");
    std::fs::create_dir(&project_root)?;
    let store = TaskStore::open(&dir.path().join("t.db")).await?;
    let task_store = store.clone();
    let database_url = crate::test_helpers::test_database_url()?;
    let runtime_store = Arc::new(
        harness_workflow::runtime::WorkflowRuntimeStore::open_with_database_url(
            &dir.path().join("workflow_runtime"),
            Some(&database_url),
        )
        .await?,
    );
    let svc = make_svc_with_workflow_runtime(store, runtime_store.clone()).await;
    let req = CreateTaskRequest {
        prompt: Some("fix the prompt-only bug".to_string()),
        project: Some(project_root.clone()),
        source: Some("dashboard".to_string()),
        external_id: Some("manual:prompt:1".to_string()),
        ..Default::default()
    };

    let task_id = svc.enqueue_background(req).await?;
    assert!(
        task_store.get_with_db_fallback(&task_id).await?.is_none(),
        "workflow runtime prompt submissions must not register legacy task rows"
    );

    let canonical_project_root = project_root.canonicalize()?;
    let workflow_id = crate::workflow_runtime_submission::prompt_workflow_id(
        &canonical_project_root.to_string_lossy(),
        Some("manual:prompt:1"),
        &task_id,
    );
    let instance = runtime_store
        .get_instance(&workflow_id)
        .await?
        .expect("runtime workflow should be recorded");
    assert_eq!(instance.definition_id, "prompt_task");
    assert_eq!(instance.state, "implementing");
    assert_eq!(instance.data["task_id"], task_id.0);
    assert!(instance.data.get("prompt").is_none());
    assert_eq!(instance.data["prompt_summary"], "prompt task");
    assert_eq!(
        instance.data["prompt_chars"],
        "fix the prompt-only bug".chars().count()
    );
    let prompt_ref = instance.data["prompt_ref"]
        .as_str()
        .expect("prompt ref should be persisted");
    assert_eq!(
        crate::workflow_runtime_submission::lookup_prompt_submission_prompt(prompt_ref).as_deref(),
        Some("fix the prompt-only bug")
    );
    assert_eq!(instance.data["source"], "dashboard");
    assert_eq!(instance.data["external_id"], "manual:prompt:1");
    assert_eq!(instance.data["execution_path"], "workflow_runtime");
    let commands = runtime_store.commands_for(&workflow_id).await?;
    assert_eq!(commands.len(), 1);
    assert_eq!(commands[0].status, "pending");
    assert_eq!(
        commands[0].command.activity_name(),
        Some("implement_prompt")
    );
    assert!(commands[0].command.command.get("prompt").is_none());
    assert_eq!(commands[0].command.command["prompt_ref"], prompt_ref);
    assert_eq!(runtime_store.pending_commands(10).await?.len(), 1);
    Ok(())
}

#[tokio::test]
async fn enqueue_background_pr_feedback_uses_bound_workflow_runtime_without_task_runner(
) -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let project_root = dir.path().join("project");
    std::fs::create_dir(&project_root)?;
    let canonical_project_root = project_root.canonicalize()?;
    let project_id = canonical_project_root.to_string_lossy().into_owned();
    let store = TaskStore::open(&dir.path().join("t.db")).await?;
    let task_store = store.clone();
    let database_url = crate::test_helpers::test_database_url()?;
    let runtime_store = Arc::new(
        harness_workflow::runtime::WorkflowRuntimeStore::open_with_database_url(
            &dir.path().join("workflow_runtime"),
            Some(&database_url),
        )
        .await?,
    );
    let workflow_id =
        harness_workflow::issue_lifecycle::workflow_id(&project_id, Some("owner/repo"), 42);
    let instance = harness_workflow::runtime::WorkflowInstance::new(
        harness_workflow::runtime::GITHUB_ISSUE_PR_DEFINITION_ID,
        1,
        "pr_open",
        harness_workflow::runtime::WorkflowSubject::new("issue", "issue:42"),
    )
    .with_id(workflow_id.clone())
    .with_data(serde_json::json!({
        "project_id": project_id,
        "repo": "owner/repo",
        "issue_number": 42,
        "task_id": "runtime-issue-task",
        "pr_number": 77,
        "pr_url": "https://github.com/owner/repo/pull/77",
        "execution_path": "workflow_runtime"
    }));
    runtime_store.upsert_instance(&instance).await?;
    let svc = make_svc_with_workflow_runtime(store, runtime_store.clone()).await;
    let req = CreateTaskRequest {
        pr: Some(77),
        repo: Some("owner/repo".to_string()),
        project: Some(project_root),
        ..Default::default()
    };

    let task_id = svc.enqueue_background(req).await?;

    assert_eq!(task_id.as_str(), "runtime-issue-task");
    assert!(
        task_store.get_with_db_fallback(&task_id).await?.is_none(),
        "workflow runtime PR feedback submissions must not register legacy PR task rows"
    );
    let updated = runtime_store
        .get_instance(&workflow_id)
        .await?
        .expect("bound issue workflow should still exist");
    assert_eq!(updated.state, "local_review_gate");
    let commands = runtime_store.commands_for(&workflow_id).await?;
    assert_eq!(commands.len(), 1);
    assert_eq!(commands[0].status, "pending");
    assert_eq!(
        commands[0].command.activity_name(),
        Some(harness_workflow::runtime::LOCAL_REVIEW_ACTIVITY)
    );
    assert_eq!(runtime_store.pending_commands(10).await?.len(), 1);
    Ok(())
}

#[tokio::test]
async fn enqueue_background_pr_feedback_creates_pr_scoped_runtime_workflow() -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let project_root = dir.path().join("project");
    std::fs::create_dir(&project_root)?;
    let canonical_project_root = project_root.canonicalize()?;
    let project_id = canonical_project_root.to_string_lossy().into_owned();
    let store = TaskStore::open(&dir.path().join("t.db")).await?;
    let task_store = store.clone();
    let database_url = crate::test_helpers::test_database_url()?;
    let runtime_store = Arc::new(
        harness_workflow::runtime::WorkflowRuntimeStore::open_with_database_url(
            &dir.path().join("workflow_runtime"),
            Some(&database_url),
        )
        .await?,
    );
    let svc = make_svc_with_workflow_runtime(store, runtime_store.clone()).await;
    let req = CreateTaskRequest {
        pr: Some(77),
        repo: Some("owner/repo".to_string()),
        project: Some(project_root),
        ..Default::default()
    };

    let task_id = svc.enqueue_background(req).await?;

    assert_eq!(
        task_id.as_str(),
        format!("repo-backlog::{project_id}::repo:owner/repo::pr:77:feedback")
    );
    assert!(
        task_store.get_with_db_fallback(&task_id).await?.is_none(),
        "workflow runtime PR feedback submissions must not register legacy PR task rows"
    );
    let workflow_id = format!("{project_id}::repo:owner/repo::pr:77:feedback");
    let workflow = runtime_store
        .get_instance(&workflow_id)
        .await?
        .expect("PR-scoped runtime workflow should be persisted");
    assert_eq!(workflow.subject.subject_type, "pr");
    assert_eq!(workflow.subject.subject_key, "pr:77");
    assert_eq!(workflow.state, "local_review_gate");
    assert_eq!(workflow.data["pr_number"], 77);
    assert!(workflow.data.get("issue_number").is_none());
    let commands = runtime_store.commands_for(&workflow_id).await?;
    assert_eq!(commands.len(), 1);
    assert_eq!(
        commands[0].command.activity_name(),
        Some(harness_workflow::runtime::LOCAL_REVIEW_ACTIVITY)
    );
    Ok(())
}

#[tokio::test]
async fn enqueue_background_prompt_submission_reopens_blocked_runtime_workflow(
) -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let project_root = dir.path().join("project");
    std::fs::create_dir(&project_root)?;
    let store = TaskStore::open(&dir.path().join("t.db")).await?;
    let database_url = crate::test_helpers::test_database_url()?;
    let runtime_store = Arc::new(
        harness_workflow::runtime::WorkflowRuntimeStore::open_with_database_url(
            &dir.path().join("workflow_runtime"),
            Some(&database_url),
        )
        .await?,
    );
    let canonical_project_root = project_root.canonicalize()?;
    let project_id = canonical_project_root.to_string_lossy().into_owned();
    let external_id = "manual:prompt:retry";
    let workflow_id = crate::workflow_runtime_submission::prompt_workflow_id(
        &project_id,
        Some(external_id),
        &TaskId::from_str(external_id),
    );
    runtime_store
        .upsert_instance(
            &harness_workflow::runtime::WorkflowInstance::new(
                harness_workflow::runtime::PROMPT_TASK_DEFINITION_ID,
                1,
                "blocked",
                harness_workflow::runtime::WorkflowSubject::new("prompt", external_id),
            )
            .with_id(workflow_id.clone())
            .with_data(serde_json::json!({
                "project_id": project_id,
                "task_id": "old-prompt-task",
                "source": "dashboard",
                "external_id": external_id,
                "prompt_ref": "old-prompt-ref",
            })),
        )
        .await?;
    let task_store = store.clone();
    let svc = make_svc_with_workflow_runtime(store, runtime_store.clone()).await;
    let req = CreateTaskRequest {
        prompt: Some("retry prompt payload after cache loss".to_string()),
        project: Some(project_root.clone()),
        source: Some("dashboard".to_string()),
        external_id: Some(external_id.to_string()),
        ..Default::default()
    };

    let task_id = svc.enqueue_background(req).await?;
    assert!(
        task_store.get_with_db_fallback(&task_id).await?.is_none(),
        "runtime prompt retry must not register a legacy task row"
    );

    let instance = runtime_store
        .get_instance(&workflow_id)
        .await?
        .expect("blocked workflow should be reopened");
    assert_eq!(instance.state, "implementing");
    assert_eq!(instance.data["task_id"], task_id.as_str());
    assert_eq!(
        instance.data["task_ids"],
        serde_json::json!(["old-prompt-task", task_id.as_str()])
    );
    assert!(instance.data.get("prompt").is_none());
    let prompt_ref = instance.data["prompt_ref"]
        .as_str()
        .expect("prompt ref should be refreshed");
    assert_ne!(prompt_ref, "old-prompt-ref");
    assert_eq!(
        crate::workflow_runtime_submission::lookup_prompt_submission_prompt(prompt_ref).as_deref(),
        Some("retry prompt payload after cache loss")
    );
    let commands = runtime_store.commands_for(&workflow_id).await?;
    assert_eq!(commands.len(), 1);
    assert_eq!(commands[0].status, "pending");
    assert_eq!(
        commands[0].command.activity_name(),
        Some("implement_prompt")
    );
    assert_eq!(commands[0].command.command["prompt_ref"], prompt_ref);
    Ok(())
}

#[tokio::test]
async fn enqueue_background_prompt_submission_preserves_runtime_dependencies() -> anyhow::Result<()>
{
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let project_root = dir.path().join("project");
    std::fs::create_dir(&project_root)?;
    let store = TaskStore::open(&dir.path().join("t.db")).await?;
    let dep_id = TaskId::from_str("dep-prompt-runtime");
    let dep = crate::task_runner::TaskState::new(dep_id.clone());
    store.insert(&dep).await;
    let database_url = crate::test_helpers::test_database_url()?;
    let runtime_store = Arc::new(
        harness_workflow::runtime::WorkflowRuntimeStore::open_with_database_url(
            &dir.path().join("workflow_runtime"),
            Some(&database_url),
        )
        .await?,
    );
    let svc = make_svc_with_workflow_runtime(store, runtime_store.clone()).await;
    let req = CreateTaskRequest {
        prompt: Some("wait for dependency before prompt implementation".to_string()),
        project: Some(project_root.clone()),
        external_id: Some("manual:prompt:blocked".to_string()),
        depends_on: vec![dep_id],
        ..Default::default()
    };

    let task_id = svc.enqueue_background(req).await?;
    let canonical_project_root = project_root.canonicalize()?;
    let workflow_id = crate::workflow_runtime_submission::prompt_workflow_id(
        &canonical_project_root.to_string_lossy(),
        Some("manual:prompt:blocked"),
        &task_id,
    );
    let instance = runtime_store
        .get_instance(&workflow_id)
        .await?
        .expect("runtime workflow should be recorded");

    assert_eq!(instance.state, "awaiting_dependencies");
    assert_eq!(instance.data["task_id"], task_id.0);
    assert_eq!(
        instance.data["depends_on"],
        serde_json::json!(["dep-prompt-runtime"])
    );
    assert!(runtime_store.commands_for(&workflow_id).await?.is_empty());
    Ok(())
}

#[tokio::test]
async fn runtime_prompt_serialization_dependency_blocks_until_predecessor_terminal(
) -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = TaskStore::open(&dir.path().join("t.db")).await?;
    let database_url = crate::test_helpers::test_database_url()?;
    let runtime_store = Arc::new(
        harness_workflow::runtime::WorkflowRuntimeStore::open_with_database_url(
            &dir.path().join("workflow_runtime"),
            Some(&database_url),
        )
        .await?,
    );
    let svc = make_svc_with_workflow_runtime(store, runtime_store.clone()).await;
    let dep_id = TaskId::from_str("runtime-prompt-serialization-waiting");
    let workflow_id = "runtime-prompt-serialization-waiting-workflow";
    let mut predecessor = harness_workflow::runtime::WorkflowInstance::new(
        harness_workflow::runtime::PROMPT_TASK_DEFINITION_ID,
        1,
        "implementing",
        harness_workflow::runtime::WorkflowSubject::new("prompt", dep_id.as_str()),
    )
    .with_id(workflow_id)
    .with_data(serde_json::json!({
        "task_id": dep_id.as_str(),
        "task_ids": [dep_id.as_str()],
    }));
    runtime_store.upsert_instance(&predecessor).await?;

    let req = CreateTaskRequest {
        serialization_depends_on: vec![dep_id.clone()],
        ..Default::default()
    };
    assert!(svc.serialization_dependencies_blocked(&req).await?);

    predecessor.state = "failed".to_string();
    runtime_store.upsert_instance(&predecessor).await?;
    assert!(!svc.serialization_dependencies_blocked(&req).await?);
    Ok(())
}

#[tokio::test]
async fn enqueue_background_issue_submission_rejects_when_runtime_loops_disabled(
) -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let project_root = dir.path().join("project");
    let server_root = dir.path().join("server-root");
    std::fs::create_dir(&project_root)?;
    std::fs::create_dir(&server_root)?;
    std::fs::write(
        project_root.join("WORKFLOW.md"),
        "---\nruntime_dispatch:\n  enabled: false\nruntime_worker:\n  enabled: true\n---\n",
    )?;
    let store = TaskStore::open(&dir.path().join("t.db")).await?;
    let database_url = crate::test_helpers::test_database_url()?;
    let runtime_store = Arc::new(
        harness_workflow::runtime::WorkflowRuntimeStore::open_with_database_url(
            &dir.path().join("workflow_runtime"),
            Some(&database_url),
        )
        .await?,
    );
    let mut config = HarnessConfig::default();
    config.server.project_root = server_root;
    let svc =
        make_svc_with_workflow_runtime_agent_and_config(store, runtime_store.clone(), config).await;
    let req = CreateTaskRequest {
        issue: Some(42),
        repo: Some("owner/repo".to_string()),
        project: Some(project_root.clone()),
        ..Default::default()
    };

    let error = svc
        .enqueue_background(req)
        .await
        .expect_err("disabled runtime loops should reject issue submissions");
    let canonical_project_root = project_root.canonicalize()?;
    let workflow_id = harness_workflow::issue_lifecycle::workflow_id(
        &canonical_project_root.to_string_lossy(),
        Some("owner/repo"),
        42,
    );

    assert!(
        error.to_string().contains(
            "workflow runtime dispatch and worker must be enabled for GitHub issue submissions"
        ),
        "unexpected error: {error}"
    );
    assert!(
        runtime_store.get_instance(&workflow_id).await?.is_none(),
        "disabled runtime loops must not create a pending runtime workflow"
    );
    Ok(())
}

#[tokio::test]
async fn enqueue_background_issue_submission_uses_request_project_runtime_policy(
) -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let project_root = dir.path().join("project");
    let server_root = dir.path().join("server-root");
    std::fs::create_dir(&project_root)?;
    std::fs::create_dir(&server_root)?;
    std::fs::write(
        server_root.join("WORKFLOW.md"),
        "---\nruntime_dispatch:\n  enabled: false\nruntime_worker:\n  enabled: true\n---\n",
    )?;
    let store = TaskStore::open(&dir.path().join("t.db")).await?;
    let task_store = store.clone();
    let database_url = crate::test_helpers::test_database_url()?;
    let runtime_store = Arc::new(
        harness_workflow::runtime::WorkflowRuntimeStore::open_with_database_url(
            &dir.path().join("workflow_runtime"),
            Some(&database_url),
        )
        .await?,
    );
    let mut config = HarnessConfig::default();
    config.server.project_root = server_root;
    let svc =
        make_svc_with_workflow_runtime_agent_and_config(store, runtime_store.clone(), config).await;
    let req = CreateTaskRequest {
        issue: Some(43),
        repo: Some("owner/repo".to_string()),
        project: Some(project_root.clone()),
        ..Default::default()
    };

    let task_id = svc.enqueue_background(req).await?;
    assert!(
        task_store.get_with_db_fallback(&task_id).await?.is_none(),
        "enabled request project runtime loops should bypass the task runner path"
    );
    let canonical_project_root = project_root.canonicalize()?;
    let workflow_id = harness_workflow::issue_lifecycle::workflow_id(
        &canonical_project_root.to_string_lossy(),
        Some("owner/repo"),
        43,
    );
    let instance = runtime_store
        .get_instance(&workflow_id)
        .await?
        .expect("runtime workflow should be recorded for the request project");
    assert_eq!(instance.state, "implementing");
    assert_eq!(instance.data["task_id"], task_id.0);
    Ok(())
}

#[tokio::test]
async fn enqueue_background_issue_submission_reuses_active_runtime_handle() -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let project_root = dir.path().join("project");
    std::fs::create_dir(&project_root)?;
    let store = TaskStore::open(&dir.path().join("t.db")).await?;
    let database_url = crate::test_helpers::test_database_url()?;
    let runtime_store = Arc::new(
        harness_workflow::runtime::WorkflowRuntimeStore::open_with_database_url(
            &dir.path().join("workflow_runtime"),
            Some(&database_url),
        )
        .await?,
    );
    let svc = make_svc_with_workflow_runtime(store, runtime_store.clone()).await;
    let req = CreateTaskRequest {
        issue: Some(42),
        repo: Some("owner/repo".to_string()),
        project: Some(project_root.clone()),
        ..Default::default()
    };

    let first = svc.enqueue_background(req.clone()).await?;
    let second = svc.enqueue_background(req).await?;

    assert_eq!(second, first);
    let canonical_project_root = project_root.canonicalize()?;
    let workflow_id = harness_workflow::issue_lifecycle::workflow_id(
        &canonical_project_root.to_string_lossy(),
        Some("owner/repo"),
        42,
    );
    assert_eq!(runtime_store.commands_for(&workflow_id).await?.len(), 1);
    Ok(())
}

#[tokio::test]
async fn enqueue_background_issue_submission_reuses_active_legacy_issue_task_before_runtime(
) -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let project_root = dir.path().join("project");
    std::fs::create_dir(&project_root)?;
    let canonical_project_root = project_root.canonicalize()?;
    let project_id = canonical_project_root.to_string_lossy().into_owned();
    let store = TaskStore::open(&dir.path().join("t.db")).await?;
    let database_url = crate::test_helpers::test_database_url()?;
    let issue_store = Arc::new(
        harness_workflow::issue_lifecycle::IssueWorkflowStore::open_with_database_url(
            &dir.path().join("issue_workflows"),
            Some(&database_url),
        )
        .await?,
    );
    let runtime_store = Arc::new(
        harness_workflow::runtime::WorkflowRuntimeStore::open_with_database_url(
            &dir.path().join("workflow_runtime"),
            Some(&database_url),
        )
        .await?,
    );
    let legacy_req = CreateTaskRequest {
        issue: Some(42),
        repo: Some("owner/repo".to_string()),
        project: Some(canonical_project_root.clone()),
        external_id: Some("issue:42".to_string()),
        ..Default::default()
    };
    let legacy_task_id = task_runner::register_pending_task(store.clone(), &legacy_req).await;
    issue_store
        .record_issue_scheduled(
            &project_id,
            Some("owner/repo"),
            42,
            legacy_task_id.as_str(),
            &[],
            false,
        )
        .await?;
    let svc =
        make_svc_with_issue_workflow_and_runtime(store, issue_store, runtime_store.clone()).await;
    let req = CreateTaskRequest {
        issue: Some(42),
        repo: Some("owner/repo".to_string()),
        project: Some(project_root.clone()),
        ..Default::default()
    };

    let task_id = svc.enqueue_background(req).await?;

    assert_eq!(task_id, legacy_task_id);
    let workflow_id =
        harness_workflow::issue_lifecycle::workflow_id(&project_id, Some("owner/repo"), 42);
    assert!(
        runtime_store.get_instance(&workflow_id).await?.is_none(),
        "legacy duplicate reuse should not create a parallel runtime workflow"
    );
    Ok(())
}

#[tokio::test]
async fn enqueue_background_issue_submission_preserves_runtime_dependencies() -> anyhow::Result<()>
{
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let project_root = dir.path().join("project");
    std::fs::create_dir(&project_root)?;
    let store = TaskStore::open(&dir.path().join("t.db")).await?;
    let dep_id = TaskId::from_str("dep-issue-runtime");
    let dep = crate::task_runner::TaskState::new(dep_id.clone());
    store.insert(&dep).await;
    let database_url = crate::test_helpers::test_database_url()?;
    let runtime_store = Arc::new(
        harness_workflow::runtime::WorkflowRuntimeStore::open_with_database_url(
            &dir.path().join("workflow_runtime"),
            Some(&database_url),
        )
        .await?,
    );
    let svc = make_svc_with_workflow_runtime(store, runtime_store.clone()).await;
    let req = CreateTaskRequest {
        issue: Some(43),
        repo: Some("owner/repo".to_string()),
        project: Some(project_root.clone()),
        depends_on: vec![dep_id],
        ..Default::default()
    };

    let task_id = svc.enqueue_background(req).await?;
    let canonical_project_root = project_root.canonicalize()?;
    let workflow_id = harness_workflow::issue_lifecycle::workflow_id(
        &canonical_project_root.to_string_lossy(),
        Some("owner/repo"),
        43,
    );
    let instance = runtime_store
        .get_instance(&workflow_id)
        .await?
        .expect("runtime workflow should be recorded");

    assert_eq!(instance.state, "awaiting_dependencies");
    assert_eq!(instance.data["task_id"], task_id.0);
    assert_eq!(
        instance.data["depends_on"],
        serde_json::json!(["dep-issue-runtime"])
    );
    assert!(runtime_store.commands_for(&workflow_id).await?.is_empty());
    Ok(())
}

#[tokio::test]
async fn enqueue_background_issue_submission_accepts_completed_runtime_dependency(
) -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let project_root = dir.path().join("project");
    std::fs::create_dir(&project_root)?;
    let store = TaskStore::open(&dir.path().join("t.db")).await?;
    let database_url = crate::test_helpers::test_database_url()?;
    let runtime_store = Arc::new(
        harness_workflow::runtime::WorkflowRuntimeStore::open_with_database_url(
            &dir.path().join("workflow_runtime"),
            Some(&database_url),
        )
        .await?,
    );
    let svc = make_svc_with_workflow_runtime(store, runtime_store.clone()).await;
    let dep_req = CreateTaskRequest {
        issue: Some(44),
        repo: Some("owner/repo".to_string()),
        project: Some(project_root.clone()),
        ..Default::default()
    };
    let dep_id = svc.enqueue_background(dep_req).await?;
    let canonical_project_root = project_root.canonicalize()?;
    let dep_workflow_id = harness_workflow::issue_lifecycle::workflow_id(
        &canonical_project_root.to_string_lossy(),
        Some("owner/repo"),
        44,
    );
    let mut dep_workflow = runtime_store
        .get_instance(&dep_workflow_id)
        .await?
        .expect("dependency workflow should be recorded");
    dep_workflow.state = "done".to_string();
    runtime_store.upsert_instance(&dep_workflow).await?;
    let dep_handle = dep_id.as_str().to_string();

    let dependent_req = CreateTaskRequest {
        issue: Some(45),
        repo: Some("owner/repo".to_string()),
        project: Some(project_root.clone()),
        depends_on: vec![dep_id],
        ..Default::default()
    };
    let dependent_id = svc.enqueue_background(dependent_req).await?;
    let dependent_workflow_id = harness_workflow::issue_lifecycle::workflow_id(
        &canonical_project_root.to_string_lossy(),
        Some("owner/repo"),
        45,
    );
    let workflow = runtime_store
        .get_instance(&dependent_workflow_id)
        .await?
        .expect("dependent workflow should be recorded");
    assert_eq!(workflow.state, "implementing");
    assert_eq!(workflow.data["task_id"], dependent_id.0);
    assert_eq!(workflow.data["depends_on"], serde_json::json!([dep_handle]));
    assert_eq!(
        runtime_store
            .commands_for(&dependent_workflow_id)
            .await?
            .len(),
        1
    );
    Ok(())
}

// ── helpers ──────────────────────────────────────────────────────────────

async fn make_event_store_noop() -> Arc<harness_observe::event_store::EventStore> {
    let path = tempfile::tempdir().unwrap().keep();
    Arc::new(
        harness_observe::event_store::EventStore::new(&path)
            .await
            .unwrap(),
    )
}

fn make_task_queue() -> Arc<TaskQueue> {
    Arc::new(TaskQueue::new(
        &harness_core::config::misc::ConcurrencyConfig::default(),
    ))
}

#[test]
fn resolve_effective_review_config_applies_project_review_override() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let harness_dir = dir.path().join(".harness");
    std::fs::create_dir(&harness_dir)?;
    std::fs::write(
        harness_dir.join("config.toml"),
        r#"
            [review]
            enabled = true
            review_bot_auto_trigger = false
        "#,
    )?;
    let mut server_config = HarnessConfig::default();
    server_config.agents.review.enabled = false;
    server_config.agents.review.review_bot_auto_trigger = true;

    let review_config = resolve_effective_review_config(&server_config, dir.path())?;

    assert!(review_config.enabled);
    assert!(!review_config.review_bot_auto_trigger);
    assert_eq!(review_config.reviewer_agent, "codex");
    Ok(())
}

async fn make_minimal_svc(
    store: Arc<TaskStore>,
    registry: Option<Arc<ProjectRegistry>>,
) -> Arc<DefaultExecutionService> {
    let config = Arc::new(HarnessConfig::default());
    let agent_registry = Arc::new(AgentRegistry::new("test"));
    DefaultExecutionService::new(
        store,
        agent_registry,
        config,
        Default::default(),
        make_event_store_noop().await,
        vec![],
        None,
        make_task_queue(),
        make_task_queue(),
        None,
        None,
        None,
        registry,
        vec![],
    )
}

async fn make_svc_with_allowed_roots(
    store: Arc<TaskStore>,
    allowed: Vec<PathBuf>,
) -> Arc<DefaultExecutionService> {
    let config = Arc::new(HarnessConfig::default());
    let agent_registry = Arc::new(AgentRegistry::new("test"));
    DefaultExecutionService::new(
        store,
        agent_registry,
        config,
        Default::default(),
        make_event_store_noop().await,
        vec![],
        None,
        make_task_queue(),
        make_task_queue(),
        None,
        None,
        None,
        None,
        allowed,
    )
}

async fn make_svc_with_agent_without_workflow_runtime(
    store: Arc<TaskStore>,
) -> Arc<DefaultExecutionService> {
    let config = Arc::new(HarnessConfig::default());
    let mut agent_registry = AgentRegistry::new("test");
    agent_registry.register("test", Arc::new(NoopAgent));
    DefaultExecutionService::new(
        store,
        Arc::new(agent_registry),
        config,
        Default::default(),
        make_event_store_noop().await,
        vec![],
        None,
        make_task_queue(),
        make_task_queue(),
        None,
        None,
        None,
        None,
        vec![],
    )
}

async fn make_svc_with_workflow_runtime(
    store: Arc<TaskStore>,
    runtime_store: Arc<harness_workflow::runtime::WorkflowRuntimeStore>,
) -> Arc<DefaultExecutionService> {
    let config = Arc::new(HarnessConfig::default());
    let agent_registry = Arc::new(AgentRegistry::new("test"));
    DefaultExecutionService::new(
        store,
        agent_registry,
        config,
        Default::default(),
        make_event_store_noop().await,
        vec![],
        None,
        make_task_queue(),
        make_task_queue(),
        None,
        None,
        Some(runtime_store),
        None,
        vec![],
    )
}

async fn make_svc_with_workflow_runtime_agent_and_config(
    store: Arc<TaskStore>,
    runtime_store: Arc<harness_workflow::runtime::WorkflowRuntimeStore>,
    config: HarnessConfig,
) -> Arc<DefaultExecutionService> {
    let mut agent_registry = AgentRegistry::new("test");
    agent_registry.register("test", Arc::new(NoopAgent));
    DefaultExecutionService::new(
        store,
        Arc::new(agent_registry),
        Arc::new(config),
        Default::default(),
        make_event_store_noop().await,
        vec![],
        None,
        make_task_queue(),
        make_task_queue(),
        None,
        None,
        Some(runtime_store),
        None,
        vec![],
    )
}

async fn make_svc_with_issue_workflow_and_runtime(
    store: Arc<TaskStore>,
    issue_store: Arc<harness_workflow::issue_lifecycle::IssueWorkflowStore>,
    runtime_store: Arc<harness_workflow::runtime::WorkflowRuntimeStore>,
) -> Arc<DefaultExecutionService> {
    let config = Arc::new(HarnessConfig::default());
    let agent_registry = Arc::new(AgentRegistry::new("test"));
    DefaultExecutionService::new(
        store,
        agent_registry,
        config,
        Default::default(),
        make_event_store_noop().await,
        vec![],
        None,
        make_task_queue(),
        make_task_queue(),
        None,
        Some(issue_store),
        Some(runtime_store),
        None,
        vec![],
    )
}
