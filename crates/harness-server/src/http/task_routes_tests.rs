use super::*;
use harness_workflow::issue_lifecycle::{IssueLifecycleState, IssueWorkflowInstance};

#[test]
fn batch_request_deserializes_issues_format() {
    let json = r#"{"issues": [300, 301, 302], "agent": "claude", "max_rounds": 3, "turn_timeout_secs": 600}"#;
    let req: BatchCreateTaskRequest = serde_json::from_str(json).unwrap();
    assert_eq!(req.issues, Some(vec![300, 301, 302]));
    assert_eq!(req.agent.as_deref(), Some("claude"));
    assert_eq!(req.max_rounds, Some(3));
    assert_eq!(req.turn_timeout_secs, Some(600));
    assert!(req.tasks.is_none());
}

#[test]
fn batch_request_deserializes_tasks_format() {
    let json = r#"{"tasks": [{"description": "fix bug X", "issue": 300}, {"description": "add feature Y", "issue": 301}]}"#;
    let req: BatchCreateTaskRequest = serde_json::from_str(json).unwrap();
    let tasks = req.tasks.unwrap();
    assert_eq!(tasks.len(), 2);
    assert_eq!(tasks[0].description.as_deref(), Some("fix bug X"));
    assert_eq!(tasks[0].issue, Some(300));
    assert_eq!(tasks[1].description.as_deref(), Some("add feature Y"));
    assert_eq!(tasks[1].issue, Some(301));
    assert!(req.issues.is_none());
}

#[test]
fn batch_request_deserializes_tasks_without_issue() {
    let json = r#"{"tasks": [{"description": "refactor module"}]}"#;
    let req: BatchCreateTaskRequest = serde_json::from_str(json).unwrap();
    let tasks = req.tasks.unwrap();
    assert_eq!(tasks[0].description.as_deref(), Some("refactor module"));
    assert!(tasks[0].issue.is_none());
}

#[test]
fn batch_request_empty_issues_list() {
    let json = r#"{"issues": []}"#;
    let req: BatchCreateTaskRequest = serde_json::from_str(json).unwrap();
    let has_issues = req.issues.as_ref().is_some_and(|v| !v.is_empty());
    assert!(!has_issues);
}

#[test]
fn batch_request_neither_issues_nor_tasks() {
    let json = r#"{"agent": "claude"}"#;
    let req: BatchCreateTaskRequest = serde_json::from_str(json).unwrap();
    let has_issues = req.issues.as_ref().is_some_and(|v| !v.is_empty());
    let has_tasks = req.tasks.as_ref().is_some_and(|v| !v.is_empty());
    assert!(!has_issues && !has_tasks);
}

#[test]
fn batch_request_deserializes_project_field() {
    let json = r#"{"issues": [1, 2], "project": "/home/user/my-repo"}"#;
    let req: BatchCreateTaskRequest = serde_json::from_str(json).unwrap();
    assert_eq!(
        req.project,
        Some(std::path::PathBuf::from("/home/user/my-repo"))
    );
}

#[test]
fn batch_request_project_defaults_to_none() {
    let json = r#"{"issues": [1]}"#;
    let req: BatchCreateTaskRequest = serde_json::from_str(json).unwrap();
    assert!(req.project.is_none());
}

#[tokio::test]
async fn resolve_project_from_registry_passes_through_none() {
    let result = resolve_project_from_registry(None, None).await;
    assert!(result.unwrap().0.is_none());
}

#[tokio::test]
async fn resolve_project_from_registry_passes_through_existing_dir() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().to_path_buf();
    let result = resolve_project_from_registry(None, Some(path.clone())).await;
    assert_eq!(result.unwrap().0, Some(path));
}

#[tokio::test]
async fn resolve_project_from_registry_existing_dir_uses_registered_metadata() {
    let dir = tempfile::tempdir().unwrap();
    let registry = crate::project_registry::ProjectRegistry::open(&dir.path().join("p.db"))
        .await
        .unwrap();
    let project_root = dir.path().join("registered-root");
    std::fs::create_dir_all(&project_root).unwrap();
    let canonical_root = project_root.canonicalize().unwrap();
    registry
        .register(crate::project_registry::Project {
            id: "registered-root".to_string(),
            root: canonical_root.clone(),
            name: Some("registered-root".to_string()),
            max_concurrent: Some(2),
            default_agent: Some("opus".to_string()),
            active: true,
            created_at: "2026-01-01T00:00:00Z".to_string(),
        })
        .await
        .unwrap();

    let result = resolve_project_from_registry(Some(&registry), Some(project_root)).await;
    let (path, agent) = result.unwrap();
    assert_eq!(path, Some(canonical_root));
    assert_eq!(agent.as_deref(), Some("opus"));
}

#[tokio::test]
async fn resolve_project_from_registry_no_registry_passes_through_nondir() {
    let path = std::path::PathBuf::from("/nonexistent/path");
    let result = resolve_project_from_registry(None, Some(path.clone())).await;
    assert_eq!(result.unwrap().0, Some(path));
}

#[tokio::test]
async fn resolve_project_from_registry_resolves_id() {
    let dir = tempfile::tempdir().unwrap();
    let registry = crate::project_registry::ProjectRegistry::open(&dir.path().join("p.db"))
        .await
        .unwrap();
    registry
        .register(crate::project_registry::Project {
            id: "my-repo".to_string(),
            root: std::path::PathBuf::from("/home/user/my-repo"),
            name: None,
            max_concurrent: None,
            default_agent: None,
            active: true,
            created_at: "2026-01-01T00:00:00Z".to_string(),
        })
        .await
        .unwrap();

    let result =
        resolve_project_from_registry(Some(&registry), Some(std::path::PathBuf::from("my-repo")))
            .await;
    let (path, agent) = result.unwrap();
    assert_eq!(path, Some(std::path::PathBuf::from("/home/user/my-repo")));
    assert_eq!(agent, None);
}

#[tokio::test]
async fn resolve_project_from_registry_returns_default_agent_from_record() {
    let dir = tempfile::tempdir().unwrap();
    let registry = crate::project_registry::ProjectRegistry::open(&dir.path().join("p.db"))
        .await
        .unwrap();
    registry
        .register(crate::project_registry::Project {
            id: "pinned-repo".to_string(),
            root: std::path::PathBuf::from("/home/user/pinned-repo"),
            name: None,
            max_concurrent: None,
            default_agent: Some("opus".to_string()),
            active: true,
            created_at: "2026-01-01T00:00:00Z".to_string(),
        })
        .await
        .unwrap();

    let result = resolve_project_from_registry(
        Some(&registry),
        Some(std::path::PathBuf::from("pinned-repo")),
    )
    .await;
    let (path, agent) = result.unwrap();
    assert_eq!(
        path,
        Some(std::path::PathBuf::from("/home/user/pinned-repo"))
    );
    assert_eq!(agent.as_deref(), Some("opus"));
}

#[tokio::test]
async fn resolve_project_from_registry_resolves_name() {
    let dir = tempfile::tempdir().unwrap();
    let registry = crate::project_registry::ProjectRegistry::open(&dir.path().join("p.db"))
        .await
        .unwrap();
    registry
        .register(crate::project_registry::Project {
            id: "named-repo-id".to_string(),
            root: std::path::PathBuf::from("/home/user/named-repo"),
            name: Some("named-repo".to_string()),
            max_concurrent: None,
            default_agent: Some("opus".to_string()),
            active: true,
            created_at: "2026-01-01T00:00:00Z".to_string(),
        })
        .await
        .unwrap();

    let result = resolve_project_from_registry(
        Some(&registry),
        Some(std::path::PathBuf::from("named-repo")),
    )
    .await;
    let (path, agent) = result.unwrap();
    assert_eq!(
        path,
        Some(std::path::PathBuf::from("/home/user/named-repo"))
    );
    assert_eq!(agent.as_deref(), Some("opus"));
}

#[tokio::test]
async fn resolve_project_from_registry_unknown_id_returns_bad_request() {
    let dir = tempfile::tempdir().unwrap();
    let registry = crate::project_registry::ProjectRegistry::open(&dir.path().join("p.db"))
        .await
        .unwrap();

    let result = resolve_project_from_registry(
        Some(&registry),
        Some(std::path::PathBuf::from("unknown-repo")),
    )
    .await;
    assert!(matches!(result, Err(EnqueueTaskError::BadRequest(_))));
}

#[test]
fn conflict_groups_empty_refs_are_singletons() {
    let refs: Vec<Vec<String>> = vec![vec![], vec![], vec![]];
    let groups = build_conflict_groups(&refs);
    assert_eq!(groups.len(), 3);
    for g in &groups {
        assert_eq!(g.len(), 1);
    }
}

#[test]
fn conflict_groups_two_tasks_share_file() {
    let refs = vec![
        vec!["src/auth.rs".to_string()],
        vec!["src/auth.rs".to_string(), "src/db.rs".to_string()],
        vec!["src/config.rs".to_string()],
    ];
    let groups = build_conflict_groups(&refs);
    assert_eq!(groups.len(), 2);
    assert!(
        groups.iter().any(|g| g.contains(&0) && g.contains(&1)),
        "tasks 0 and 1 must be in the same conflict group"
    );
    assert!(
        groups.iter().any(|g| g.len() == 1 && g[0] == 2),
        "task 2 must be a singleton group"
    );
}

#[test]
fn conflict_groups_transitive_overlap() {
    let refs = vec![
        vec!["a.rs".to_string()],
        vec!["a.rs".to_string(), "b.rs".to_string()],
        vec!["b.rs".to_string()],
    ];
    let groups = build_conflict_groups(&refs);
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0].len(), 3);
}

#[test]
fn conflict_groups_no_overlap() {
    let refs = vec![
        vec!["a.rs".to_string()],
        vec!["b.rs".to_string()],
        vec!["c.rs".to_string()],
    ];
    let groups = build_conflict_groups(&refs);
    assert_eq!(groups.len(), 3);
    for g in &groups {
        assert_eq!(g.len(), 1);
    }
}

#[test]
fn conflict_groups_single_task() {
    let refs = vec![vec!["src/main.rs".to_string()]];
    let groups = build_conflict_groups(&refs);
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0], vec![0]);
}

#[test]
fn workflow_reuse_strategy_prefers_active_task() {
    let mut workflow = IssueWorkflowInstance::new(
        "/tmp/project".to_string(),
        Some("owner/repo".to_string()),
        42,
    );
    workflow.state = IssueLifecycleState::Implementing;
    workflow.active_task_id = Some("task-123".to_string());
    workflow.pr_number = Some(7);
    match workflow_reuse_strategy(&workflow) {
        WorkflowReuseStrategy::ActiveTask(task_id) => assert_eq!(task_id.0, "task-123"),
        _ => panic!("expected active-task reuse strategy"),
    }
}

#[test]
fn workflow_reuse_strategy_falls_back_to_pr_when_active_task_missing() {
    let mut workflow = IssueWorkflowInstance::new(
        "/tmp/project".to_string(),
        Some("owner/repo".to_string()),
        42,
    );
    workflow.state = IssueLifecycleState::AwaitingFeedback;
    workflow.pr_number = Some(99);
    match workflow_reuse_strategy(&workflow) {
        WorkflowReuseStrategy::PrExternalId(ext_id) => assert_eq!(ext_id, "pr:99"),
        _ => panic!("expected pr-external-id reuse strategy"),
    }
}

#[test]
fn workflow_reuse_strategy_rejects_failed_and_cancelled_workflows() {
    let mut failed = IssueWorkflowInstance::new(
        "/tmp/project".to_string(),
        Some("owner/repo".to_string()),
        1,
    );
    failed.state = IssueLifecycleState::Failed;
    failed.active_task_id = Some("task-failed".to_string());
    assert!(matches!(
        workflow_reuse_strategy(&failed),
        WorkflowReuseStrategy::None
    ));

    let mut cancelled = IssueWorkflowInstance::new(
        "/tmp/project".to_string(),
        Some("owner/repo".to_string()),
        2,
    );
    cancelled.state = IssueLifecycleState::Cancelled;
    cancelled.pr_number = Some(88);
    assert!(matches!(
        workflow_reuse_strategy(&cancelled),
        WorkflowReuseStrategy::None
    ));
}

// ── select_agent three-tier precedence tests ─────────────────────────────

use harness_agents::registry::AgentRegistry;
use harness_core::{
    agent::{AgentRequest, AgentResponse, CodeAgent, StreamItem},
    types::{Capability, TokenUsage},
};

struct StubAgent {
    name: &'static str,
}

#[async_trait::async_trait]
impl CodeAgent for StubAgent {
    fn name(&self) -> &str {
        self.name
    }
    fn capabilities(&self) -> Vec<Capability> {
        vec![]
    }
    async fn execute(&self, _req: AgentRequest) -> harness_core::error::Result<AgentResponse> {
        Ok(AgentResponse {
            output: String::new(),
            stderr: String::new(),
            items: vec![],
            token_usage: TokenUsage {
                input_tokens: 0,
                output_tokens: 0,
                total_tokens: 0,
                cost_usd: 0.0,
            },
            model: self.name.to_string(),
            exit_code: Some(0),
        })
    }
    async fn execute_stream(
        &self,
        _req: AgentRequest,
        _tx: tokio::sync::mpsc::Sender<StreamItem>,
    ) -> harness_core::error::Result<()> {
        Ok(())
    }
}

fn registry_with(default: &str, names: &[&'static str]) -> AgentRegistry {
    let mut reg = AgentRegistry::new(default);
    for &n in names {
        reg.register(n, Arc::new(StubAgent { name: n }));
    }
    reg
}

fn req_with_agent(
    agent: Option<&str>,
    project: Option<std::path::PathBuf>,
) -> task_runner::CreateTaskRequest {
    let mut req = task_runner::CreateTaskRequest::default();
    req.agent = agent.map(str::to_owned);
    req.project = project;
    req.prompt = Some("do something".to_string());
    req
}

#[test]
fn agent_override_wins() {
    let reg = registry_with("auto", &["opus", "claude"]);

    let dir = tempfile::tempdir().unwrap();
    let req = req_with_agent(Some("opus"), Some(dir.path().to_path_buf()));

    let agent = select_agent(&req, &reg, None).unwrap();
    assert_eq!(agent.name(), "opus");
}

#[test]
fn project_default_used() {
    let reg = registry_with("auto", &["claude", "sonnet"]);

    let dir = tempfile::tempdir().unwrap();
    let harness_dir = dir.path().join(".harness");
    std::fs::create_dir_all(&harness_dir).unwrap();
    std::fs::write(
        harness_dir.join("config.toml"),
        "[agent]\ndefault = \"claude\"\n",
    )
    .unwrap();

    let req = req_with_agent(None, Some(dir.path().to_path_buf()));
    let agent = select_agent(&req, &reg, None).unwrap();
    assert_eq!(agent.name(), "claude");
}

#[test]
fn global_fallback_when_no_project_config() {
    let reg = registry_with("sonnet", &["sonnet"]);

    let dir = tempfile::tempdir().unwrap();
    let req = req_with_agent(None, Some(dir.path().to_path_buf()));

    let agent = select_agent(&req, &reg, None).unwrap();
    assert_eq!(agent.name(), "sonnet");
}

#[test]
fn global_fallback_when_project_config_has_no_agent_field() {
    let reg = registry_with("sonnet", &["sonnet"]);

    let dir = tempfile::tempdir().unwrap();
    let harness_dir = dir.path().join(".harness");
    std::fs::create_dir_all(&harness_dir).unwrap();
    std::fs::write(
        harness_dir.join("config.toml"),
        "[git]\nbase_branch = \"main\"\n",
    )
    .unwrap();

    let req = req_with_agent(None, Some(dir.path().to_path_buf()));
    let agent = select_agent(&req, &reg, None).unwrap();
    assert_eq!(agent.name(), "sonnet");
}

#[test]
fn project_pinning_same_agent_as_server_default_bypasses_complexity_dispatch() {
    let reg = registry_with("claude", &["claude", "opus"]);

    let dir = tempfile::tempdir().unwrap();
    let harness_dir = dir.path().join(".harness");
    std::fs::create_dir_all(&harness_dir).unwrap();
    std::fs::write(
        harness_dir.join("config.toml"),
        "[agent]\ndefault = \"claude\"\n",
    )
    .unwrap();

    let req = req_with_agent(None, Some(dir.path().to_path_buf()));
    let agent = select_agent(&req, &reg, None).unwrap();
    assert_eq!(agent.name(), "claude");
}

#[test]
fn unknown_agent_in_project_config_returns_bad_request() {
    let reg = registry_with("auto", &["sonnet"]);

    let dir = tempfile::tempdir().unwrap();
    let harness_dir = dir.path().join(".harness");
    std::fs::create_dir_all(&harness_dir).unwrap();
    std::fs::write(
        harness_dir.join("config.toml"),
        "[agent]\ndefault = \"nonexistent\"\n",
    )
    .unwrap();

    let req = req_with_agent(None, Some(dir.path().to_path_buf()));
    let result = select_agent(&req, &reg, None);
    assert!(matches!(result, Err(EnqueueTaskError::BadRequest(_))));
}

#[test]
fn config_toml_auto_falls_through_to_complexity_dispatch() {
    let reg = registry_with("sonnet", &["sonnet"]);

    let dir = tempfile::tempdir().unwrap();
    let harness_dir = dir.path().join(".harness");
    std::fs::create_dir_all(&harness_dir).unwrap();
    std::fs::write(
        harness_dir.join("config.toml"),
        "[agent]\ndefault = \"auto\"\n",
    )
    .unwrap();

    let req = req_with_agent(None, Some(dir.path().to_path_buf()));
    let agent = select_agent(&req, &reg, None).unwrap();
    assert_eq!(agent.name(), "sonnet");
}

#[test]
fn registry_default_agent_used_when_config_toml_has_no_agent_section() {
    let reg = registry_with("sonnet", &["sonnet", "opus"]);

    let dir = tempfile::tempdir().unwrap();
    let req = req_with_agent(None, Some(dir.path().to_path_buf()));

    let agent = select_agent(&req, &reg, Some("opus")).unwrap();
    assert_eq!(agent.name(), "opus");
}

#[test]
fn registry_auto_agent_falls_through_to_complexity_dispatch() {
    let reg = registry_with("sonnet", &["sonnet"]);

    let dir = tempfile::tempdir().unwrap();
    let req = req_with_agent(None, Some(dir.path().to_path_buf()));

    let agent = select_agent(&req, &reg, Some("auto")).unwrap();
    assert_eq!(agent.name(), "sonnet");
}
