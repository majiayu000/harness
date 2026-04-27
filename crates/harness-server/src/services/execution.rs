//! ExecutionService — task enqueue with project resolution, agent dispatch,
//! workspace allocation, concurrency management, and completion callbacks.

use crate::{
    complexity_router,
    http::resolve_reviewer,
    project_registry::{check_allowed_roots, ProjectRegistry},
    task_queue::TaskQueue,
    task_runner::{self, CompletionCallback, CreateTaskRequest, TaskId, TaskStore},
    workspace::WorkspaceManager,
};
use async_trait::async_trait;
use harness_agents::registry::AgentRegistry;
use harness_core::{agent::CodeAgent, config::HarnessConfig, interceptor::TurnInterceptor};
use harness_skills::store::SkillStore;
use harness_workflow::issue_lifecycle::{IssueLifecycleState, IssueWorkflowInstance};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;

/// Error returned by [`ExecutionService::enqueue`] and
/// [`ExecutionService::enqueue_background`].
#[derive(Debug)]
pub enum EnqueueTaskError {
    BadRequest(String),
    Internal(String),
    /// The server is inside a maintenance window; the caller should retry after
    /// `retry_after_secs` seconds.
    MaintenanceWindow {
        retry_after_secs: u64,
    },
}

impl std::fmt::Display for EnqueueTaskError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadRequest(msg) => write!(f, "bad request: {msg}"),
            Self::Internal(msg) => write!(f, "internal error: {msg}"),
            Self::MaintenanceWindow { retry_after_secs } => {
                write!(
                    f,
                    "maintenance window active; retry after {retry_after_secs}s"
                )
            }
        }
    }
}

impl std::error::Error for EnqueueTaskError {}

/// Trait interface for task execution orchestration.
///
/// Implementations handle the full lifecycle of turning a [`CreateTaskRequest`]
/// into a running task: project resolution, allowed-roots enforcement, agent
/// selection, concurrency-permit acquisition, workspace allocation, and
/// completion-callback wiring.
#[async_trait]
pub trait ExecutionService: Send + Sync {
    /// Enqueue a task and wait for a concurrency permit before returning.
    ///
    /// Blocks the caller until a slot is available, then spawns the task in
    /// the background and returns its ID.
    async fn enqueue(&self, req: CreateTaskRequest) -> Result<TaskId, EnqueueTaskError>;

    /// Register a task immediately and begin execution in the background.
    ///
    /// Returns the task ID without waiting for a concurrency permit. A
    /// background tokio task handles permit acquisition and execution, keeping
    /// HTTP handlers responsive under load.
    async fn enqueue_background(&self, req: CreateTaskRequest) -> Result<TaskId, EnqueueTaskError>;
}

/// All dependencies required to execute a task end-to-end.
pub struct DefaultExecutionService {
    tasks: Arc<TaskStore>,
    agent_registry: Arc<AgentRegistry>,
    server_config: Arc<HarnessConfig>,
    skills: Arc<RwLock<SkillStore>>,
    events: Arc<harness_observe::event_store::EventStore>,
    interceptors: Vec<Arc<dyn TurnInterceptor>>,
    workspace_mgr: Option<Arc<WorkspaceManager>>,
    task_queue: Arc<TaskQueue>,
    completion_callback: Option<CompletionCallback>,
    issue_workflow_store: Option<Arc<harness_workflow::issue_lifecycle::IssueWorkflowStore>>,
    project_registry: Option<Arc<ProjectRegistry>>,
    allowed_project_roots: Vec<PathBuf>,
}

enum WorkflowReuseStrategy {
    ActiveTask(TaskId),
    PrExternalId(String),
    None,
}

impl DefaultExecutionService {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        tasks: Arc<TaskStore>,
        agent_registry: Arc<AgentRegistry>,
        server_config: Arc<HarnessConfig>,
        skills: Arc<RwLock<SkillStore>>,
        events: Arc<harness_observe::event_store::EventStore>,
        interceptors: Vec<Arc<dyn TurnInterceptor>>,
        workspace_mgr: Option<Arc<WorkspaceManager>>,
        task_queue: Arc<TaskQueue>,
        completion_callback: Option<CompletionCallback>,
        issue_workflow_store: Option<Arc<harness_workflow::issue_lifecycle::IssueWorkflowStore>>,
        project_registry: Option<Arc<ProjectRegistry>>,
        allowed_project_roots: Vec<PathBuf>,
    ) -> Arc<Self> {
        Arc::new(Self {
            tasks,
            agent_registry,
            server_config,
            skills,
            events,
            interceptors,
            workspace_mgr,
            task_queue,
            completion_callback,
            issue_workflow_store,
            project_registry,
            allowed_project_roots,
        })
    }

    /// Resolve a project path-or-ID through the registry.
    ///
    /// - `None` → pass through (worktree auto-detection happens later).
    /// - Existing directory → pass through unchanged.
    /// - Non-directory string → treated as a project ID and looked up in the
    ///   registry. Returns `BadRequest` when the ID is not found.
    async fn resolve_project(
        &self,
        project: Option<PathBuf>,
    ) -> Result<(Option<PathBuf>, Option<String>), EnqueueTaskError> {
        let (Some(registry), Some(project_path)) =
            (self.project_registry.as_ref(), project.clone())
        else {
            return Ok((project, None));
        };

        if project_path.is_dir() {
            return Ok((Some(project_path), None));
        }

        let id = project_path.to_string_lossy();
        match registry.get(&id).await {
            Ok(Some(project)) => return Ok((Some(project.root), project.default_agent)),
            Ok(None) => {}
            Err(e) => return Err(EnqueueTaskError::Internal(e.to_string())),
        }
        match registry.get_by_name(&id).await {
            Ok(Some(project)) => Ok((Some(project.root), project.default_agent)),
            Ok(None) => Err(EnqueueTaskError::BadRequest(format!(
                "project '{id}' not found in registry and is not a valid directory"
            ))),
            Err(e) => Err(EnqueueTaskError::Internal(e.to_string())),
        }
    }

    /// Validate the request has at least one task specifier and a valid priority.
    fn validate_request(req: &CreateTaskRequest) -> Result<(), EnqueueTaskError> {
        if req.prompt.is_none() && req.issue.is_none() && req.pr.is_none() {
            return Err(EnqueueTaskError::BadRequest(
                "at least one of prompt, issue, or pr must be provided".to_string(),
            ));
        }
        if req.priority > task_runner::MAX_TASK_PRIORITY {
            return Err(EnqueueTaskError::BadRequest(format!(
                "priority {} out of range; maximum is {} (0=normal, 1=high, 2=critical)",
                req.priority,
                task_runner::MAX_TASK_PRIORITY,
            )));
        }
        Ok(())
    }

    /// Check the resolved canonical project against `allowed_project_roots`.
    fn check_allowed_roots(&self, canonical: &std::path::Path) -> Result<(), EnqueueTaskError> {
        check_allowed_roots(canonical, &self.allowed_project_roots)
            .map_err(EnqueueTaskError::BadRequest)
    }

    fn queue_timeout_message(&self, project_id: &str) -> String {
        let diag = self.task_queue.diagnostics(project_id);
        let project_holding = diag.project_running + diag.project_awaiting_global;
        let project_full = project_holding >= diag.project_limit;
        let global_full = diag.global_running >= diag.global_limit;
        let reason = match (global_full, project_full) {
            (true, true) => "global and project capacity saturated",
            (true, false) => "global capacity saturated",
            (false, true) => "project capacity saturated",
            (false, false) => "permit wait exceeded timeout",
        };
        format!(
            "{reason} (domain=primary, global_running={}, global_queued={}, global_limit={}, project_running={}, project_waiting={}, project_awaiting_global={}, project_limit={})",
            diag.global_running,
            diag.global_queued,
            diag.global_limit,
            diag.project_running,
            diag.project_waiting_for_project,
            diag.project_awaiting_global,
            diag.project_limit,
        )
    }

    fn populate_external_id(req: &mut CreateTaskRequest) {
        match &req.external_id {
            None => {
                if let Some(issue) = req.issue {
                    req.external_id = Some(format!("issue:{issue}"));
                } else if let Some(pr) = req.pr {
                    req.external_id = Some(format!("pr:{pr}"));
                }
            }
            Some(id) => {
                if id.starts_with("issue:") || id.starts_with("pr:") {
                    return;
                }
                if id.chars().all(|c| c.is_ascii_digit()) && !id.is_empty() {
                    if req.issue.is_some() {
                        req.external_id = Some(format!("issue:{id}"));
                    } else if req.pr.is_some() {
                        req.external_id = Some(format!("pr:{id}"));
                    }
                }
            }
        }
    }

    fn workflow_state_allows_reuse(state: IssueLifecycleState) -> bool {
        !matches!(
            state,
            IssueLifecycleState::Failed | IssueLifecycleState::Cancelled
        )
    }

    fn workflow_reuse_strategy(workflow: &IssueWorkflowInstance) -> WorkflowReuseStrategy {
        if !Self::workflow_state_allows_reuse(workflow.state) {
            return WorkflowReuseStrategy::None;
        }
        if let Some(task_id) = workflow.active_task_id.as_ref() {
            return WorkflowReuseStrategy::ActiveTask(harness_core::types::TaskId(task_id.clone()));
        }
        if let Some(pr_number) = workflow.pr_number {
            return WorkflowReuseStrategy::PrExternalId(format!("pr:{pr_number}"));
        }
        WorkflowReuseStrategy::None
    }

    async fn check_workflow_duplicate(
        &self,
        project_id: &str,
        req: &CreateTaskRequest,
    ) -> Option<TaskId> {
        let workflows = self.issue_workflow_store.as_ref()?;
        let workflow = if let Some(issue_number) = req.issue {
            workflows
                .get_by_issue(project_id, req.repo.as_deref(), issue_number)
                .await
                .ok()
                .flatten()
        } else if let Some(pr_number) = req.pr {
            workflows
                .get_by_pr(project_id, req.repo.as_deref(), pr_number)
                .await
                .ok()
                .flatten()
        } else {
            None
        }?;

        match Self::workflow_reuse_strategy(&workflow) {
            WorkflowReuseStrategy::ActiveTask(task_id) => {
                if self
                    .tasks
                    .exists_with_db_fallback(&task_id)
                    .await
                    .unwrap_or(false)
                {
                    return Some(task_id);
                }
            }
            WorkflowReuseStrategy::PrExternalId(pr_ext_id) => {
                if let Some(task_id) = self
                    .tasks
                    .find_active_duplicate(project_id, &pr_ext_id)
                    .await
                {
                    return Some(task_id);
                }
                if let Some((task_id, _)) = self
                    .tasks
                    .find_terminal_pr_duplicate(project_id, &pr_ext_id)
                    .await
                {
                    return Some(task_id);
                }
            }
            WorkflowReuseStrategy::None => {}
        }

        None
    }

    async fn check_duplicate(&self, project_id: &str, req: &CreateTaskRequest) -> Option<TaskId> {
        let ext_id = req.external_id.as_deref()?;
        let existing_id = self.tasks.find_active_duplicate(project_id, ext_id).await?;
        tracing::info!(
            existing_task = %existing_id.0,
            external_id = %ext_id,
            "dedup: returning existing active task instead of creating duplicate"
        );
        Some(existing_id)
    }

    async fn check_pr_duplicate(
        &self,
        project_id: &str,
        req: &CreateTaskRequest,
    ) -> Option<TaskId> {
        let ext_id = req.external_id.as_deref()?;
        let (existing_id, pr_url) = self
            .tasks
            .find_terminal_pr_duplicate(project_id, ext_id)
            .await?;
        tracing::info!(
            existing_task = %existing_id.0,
            external_id = %ext_id,
            pr_url = %pr_url,
            "dedup: terminal task already created PR, returning existing task instead of creating duplicate"
        );
        Some(existing_id)
    }

    async fn dependencies_blocked(&self, req: &CreateTaskRequest) -> bool {
        if req.depends_on.is_empty() {
            return false;
        }
        for dep_id in &req.depends_on {
            if !matches!(
                self.tasks.dep_status(dep_id).await,
                Some(task_runner::TaskStatus::Done)
            ) {
                return true;
            }
        }
        false
    }

    /// Select the agent for this request (explicit, project default, or complexity-routed).
    fn select_agent(
        &self,
        req: &CreateTaskRequest,
        registry_agent: Option<&str>,
    ) -> Result<Arc<dyn CodeAgent>, EnqueueTaskError> {
        if let Some(name) = &req.agent {
            return self.agent_registry.get(name).ok_or_else(|| {
                EnqueueTaskError::BadRequest(format!("agent '{name}' not registered"))
            });
        }

        if let Some(project_root) = &req.project {
            let project_cfg = harness_core::config::project::load_project_config(project_root)
                .map_err(|e| EnqueueTaskError::Internal(e.to_string()))?;
            if let Some(agent_name) = project_cfg.agent.as_ref().and_then(|a| a.default.as_ref()) {
                if agent_name != "auto" {
                    return self.agent_registry.get(agent_name).ok_or_else(|| {
                        EnqueueTaskError::BadRequest(format!("agent '{agent_name}' not registered"))
                    });
                }
            }
        }

        if let Some(name) = registry_agent {
            if name != "auto" {
                return self.agent_registry.get(name).ok_or_else(|| {
                    EnqueueTaskError::BadRequest(format!("agent '{name}' not registered"))
                });
            }
        }

        let classification = complexity_router::classify(
            req.prompt.as_deref().unwrap_or_default(),
            req.issue,
            req.pr,
        );
        self.agent_registry
            .dispatch(&classification)
            .map_err(|e| EnqueueTaskError::Internal(e.to_string()))
    }

    async fn track_issue_workflow(
        &self,
        project_id: &str,
        req: &CreateTaskRequest,
        task_id: &TaskId,
    ) {
        let Some(workflows) = self.issue_workflow_store.as_ref() else {
            return;
        };
        if let Some(issue_number) = req.issue {
            if let Err(e) = workflows
                .record_issue_scheduled(
                    project_id,
                    req.repo.as_deref(),
                    issue_number,
                    &task_id.0,
                    &req.labels,
                    req.force_execute,
                )
                .await
            {
                tracing::warn!(
                    issue = issue_number,
                    task_id = %task_id.0,
                    "issue workflow enqueue tracking failed: {e}"
                );
            }
            return;
        }
        if let Some(pr_number) = req.pr {
            if let Err(e) = workflows
                .record_feedback_task_scheduled(
                    project_id,
                    req.repo.as_deref(),
                    pr_number,
                    &task_id.0,
                )
                .await
            {
                tracing::warn!(
                    pr = pr_number,
                    task_id = %task_id.0,
                    "issue workflow PR enqueue tracking failed: {e}"
                );
            }
        }
    }
}

#[async_trait]
impl ExecutionService for DefaultExecutionService {
    async fn enqueue(&self, mut req: CreateTaskRequest) -> Result<TaskId, EnqueueTaskError> {
        let now = chrono::Utc::now();
        if self.server_config.maintenance_window.in_quiet_window(now) {
            let retry_after_secs = self
                .server_config
                .maintenance_window
                .secs_until_window_end(now);
            return Err(EnqueueTaskError::MaintenanceWindow { retry_after_secs });
        }

        Self::validate_request(&req)?;

        let (resolved_project, registry_default_agent) = self.resolve_project(req.project).await?;
        req.project = resolved_project;

        let canonical = task_runner::resolve_canonical_project(req.project.clone())
            .await
            .map_err(|e| EnqueueTaskError::Internal(e.to_string()))?;
        self.check_allowed_roots(&canonical)?;

        let project_id = canonical.to_string_lossy().into_owned();
        req.project = Some(canonical);

        task_runner::fill_missing_repo_from_project(&mut req).await;
        Self::populate_external_id(&mut req);
        if let Some(existing_id) = self.check_workflow_duplicate(&project_id, &req).await {
            return Ok(existing_id);
        }
        if let Some(existing_id) = self.check_duplicate(&project_id, &req).await {
            return Ok(existing_id);
        }
        if let Some(existing_id) = self.check_pr_duplicate(&project_id, &req).await {
            return Ok(existing_id);
        }

        if self.dependencies_blocked(&req).await {
            let workflow_req = req.clone();
            let task_id = task_runner::spawn_task_awaiting_deps(self.tasks.clone(), req)
                .await
                .map_err(|e| EnqueueTaskError::BadRequest(e.to_string()))?;
            self.track_issue_workflow(&project_id, &workflow_req, &task_id)
                .await;
            return Ok(task_id);
        }
        req.depends_on.clear();

        let permit = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            self.task_queue.acquire(&project_id, req.priority),
        )
        .await
        .map_err(|_| EnqueueTaskError::Internal(self.queue_timeout_message(&project_id)))?
        .map_err(|e| EnqueueTaskError::Internal(e.to_string()))?;

        let agent = self.select_agent(&req, registry_default_agent.as_deref())?;
        let (reviewer, _) = resolve_reviewer(
            &self.agent_registry,
            &self.server_config.agents.review,
            agent.name(),
        );

        let workflow_req = req.clone();
        let task_id = task_runner::spawn_task(
            self.tasks.clone(),
            agent,
            reviewer,
            self.server_config.clone(),
            self.skills.clone(),
            self.events.clone(),
            self.interceptors.clone(),
            req,
            self.workspace_mgr.clone(),
            permit,
            self.completion_callback.clone(),
            self.issue_workflow_store.clone(),
        )
        .await;

        self.track_issue_workflow(&project_id, &workflow_req, &task_id)
            .await;

        Ok(task_id)
    }

    async fn enqueue_background(
        &self,
        mut req: CreateTaskRequest,
    ) -> Result<TaskId, EnqueueTaskError> {
        let now = chrono::Utc::now();
        if self.server_config.maintenance_window.in_quiet_window(now) {
            let retry_after_secs = self
                .server_config
                .maintenance_window
                .secs_until_window_end(now);
            return Err(EnqueueTaskError::MaintenanceWindow { retry_after_secs });
        }

        Self::validate_request(&req)?;

        let (resolved_project, registry_default_agent) = self.resolve_project(req.project).await?;
        req.project = resolved_project;

        let canonical = task_runner::resolve_canonical_project(req.project.clone())
            .await
            .map_err(|e| EnqueueTaskError::Internal(e.to_string()))?;
        self.check_allowed_roots(&canonical)?;

        let project_id = canonical.to_string_lossy().into_owned();
        req.project = Some(canonical);
        task_runner::fill_missing_repo_from_project(&mut req).await;

        let agent = self.select_agent(&req, registry_default_agent.as_deref())?;
        let (reviewer, _) = resolve_reviewer(
            &self.agent_registry,
            &self.server_config.agents.review,
            agent.name(),
        );

        Self::populate_external_id(&mut req);
        if let Some(existing_id) = self.check_workflow_duplicate(&project_id, &req).await {
            return Ok(existing_id);
        }
        if let Some(existing_id) = self.check_duplicate(&project_id, &req).await {
            return Ok(existing_id);
        }
        if let Some(existing_id) = self.check_pr_duplicate(&project_id, &req).await {
            return Ok(existing_id);
        }

        if self.dependencies_blocked(&req).await {
            let workflow_req = req.clone();
            let task_id = task_runner::spawn_task_awaiting_deps(self.tasks.clone(), req)
                .await
                .map_err(|e| EnqueueTaskError::BadRequest(e.to_string()))?;
            self.track_issue_workflow(&project_id, &workflow_req, &task_id)
                .await;
            return Ok(task_id);
        }
        req.depends_on.clear();

        let server_config = self.server_config.clone();

        // Register the task immediately so the caller gets an ID without blocking.
        let task_id = task_runner::register_pending_task(self.tasks.clone(), &req).await;

        // Spawn a background tokio task that waits for a concurrency slot then executes.
        let tasks = self.tasks.clone();
        let skills = self.skills.clone();
        let events = self.events.clone();
        let interceptors = self.interceptors.clone();
        let workspace_mgr = self.workspace_mgr.clone();
        let task_queue = self.task_queue.clone();
        let completion_callback = self.completion_callback.clone();
        let issue_workflow_store = self.issue_workflow_store.clone();
        let task_id2 = task_id.clone();
        self.track_issue_workflow(&project_id, &req, &task_id).await;
        tokio::spawn(async move {
            match task_queue.acquire(&project_id, req.priority).await {
                Ok(permit) => {
                    task_runner::spawn_preregistered_task(
                        task_id2,
                        tasks,
                        agent,
                        reviewer,
                        server_config,
                        skills,
                        events,
                        interceptors,
                        req,
                        workspace_mgr,
                        permit,
                        completion_callback,
                        issue_workflow_store,
                        None,
                    )
                    .await;
                }
                Err(e) => {
                    if let Err(persist_err) =
                        task_runner::mutate_and_persist(&tasks, &task_id2, |s| {
                            s.status = task_runner::TaskStatus::Failed;
                            s.error = Some(format!("task queue full: {e}"));
                        })
                        .await
                    {
                        tracing::error!(
                            task_id = %task_id2.0,
                            "failed to persist task failure after queue full: {persist_err}"
                        );
                    }
                    tasks.close_task_stream(&task_id2);
                }
            }
        });

        Ok(task_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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

    #[async_trait]
    impl ExecutionService for AlwaysSucceedExecutionService {
        async fn enqueue(&self, _req: CreateTaskRequest) -> Result<TaskId, EnqueueTaskError> {
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
        let result = svc.resolve_project(Some(path.clone())).await?;
        assert_eq!(result.0, Some(path));
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

    // ── helpers ──────────────────────────────────────────────────────────────

    async fn make_event_store_noop() -> Arc<harness_observe::event_store::EventStore> {
        let dir = tempfile::tempdir().unwrap();
        Arc::new(
            harness_observe::event_store::EventStore::new(dir.path())
                .await
                .unwrap(),
        )
    }

    fn make_task_queue() -> Arc<TaskQueue> {
        Arc::new(TaskQueue::new(
            &harness_core::config::misc::ConcurrencyConfig::default(),
        ))
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
            None,
            None,
            None,
            allowed,
        )
    }
}
