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
use harness_core::{config::HarnessConfig, interceptor::TurnInterceptor};
use harness_skills::store::SkillStore;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;

/// Error returned by [`ExecutionService::enqueue`] and
/// [`ExecutionService::enqueue_background`].
#[derive(Debug)]
pub enum EnqueueTaskError {
    BadRequest(String),
    Internal(String),
}

impl std::fmt::Display for EnqueueTaskError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadRequest(msg) => write!(f, "bad request: {msg}"),
            Self::Internal(msg) => write!(f, "internal error: {msg}"),
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
    project_registry: Option<Arc<ProjectRegistry>>,
    allowed_project_roots: Vec<PathBuf>,
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
    ) -> Result<Option<PathBuf>, EnqueueTaskError> {
        match (self.project_registry.as_ref(), project) {
            (Some(registry), Some(project_path)) => {
                if project_path.is_dir() {
                    return Ok(Some(project_path));
                }
                let id = project_path.to_string_lossy();
                match registry.resolve_path(&id).await {
                    Ok(Some(root)) => Ok(Some(root)),
                    Ok(None) => Err(EnqueueTaskError::BadRequest(format!(
                        "project '{id}' not found in registry and is not a valid directory"
                    ))),
                    Err(e) => Err(EnqueueTaskError::Internal(e.to_string())),
                }
            }
            (_, project) => Ok(project),
        }
    }

    /// Validate the request has at least one task specifier.
    fn validate_request(req: &CreateTaskRequest) -> Result<(), EnqueueTaskError> {
        if req.prompt.is_none() && req.issue.is_none() && req.pr.is_none() {
            return Err(EnqueueTaskError::BadRequest(
                "at least one of prompt, issue, or pr must be provided".to_string(),
            ));
        }
        Ok(())
    }

    /// Check the resolved canonical project against `allowed_project_roots`.
    fn check_allowed_roots(&self, canonical: &std::path::Path) -> Result<(), EnqueueTaskError> {
        check_allowed_roots(canonical, &self.allowed_project_roots)
            .map_err(EnqueueTaskError::BadRequest)
    }

    /// Select the agent for this request (explicit name or complexity-routed).
    fn select_agent(
        &self,
        req: &CreateTaskRequest,
    ) -> Result<Arc<dyn harness_core::agent::CodeAgent>, EnqueueTaskError> {
        if let Some(name) = &req.agent {
            self.agent_registry.get(name).ok_or_else(|| {
                EnqueueTaskError::BadRequest(format!("agent '{name}' not registered"))
            })
        } else {
            let classification = complexity_router::classify(
                req.prompt.as_deref().unwrap_or_default(),
                req.issue,
                req.pr,
            );
            self.agent_registry
                .dispatch(&classification)
                .map_err(|e| EnqueueTaskError::Internal(e.to_string()))
        }
    }
}

#[async_trait]
impl ExecutionService for DefaultExecutionService {
    async fn enqueue(&self, mut req: CreateTaskRequest) -> Result<TaskId, EnqueueTaskError> {
        Self::validate_request(&req)?;

        req.project = self.resolve_project(req.project).await?;

        let canonical = task_runner::resolve_canonical_project(req.project.clone())
            .await
            .map_err(|e| EnqueueTaskError::Internal(e.to_string()))?;
        self.check_allowed_roots(&canonical)?;

        let project_id = canonical.to_string_lossy().into_owned();
        req.project = Some(canonical);

        let permit = self
            .task_queue
            .acquire(&project_id)
            .await
            .map_err(|e| EnqueueTaskError::Internal(e.to_string()))?;

        let agent = self.select_agent(&req)?;
        let (reviewer, _) = resolve_reviewer(
            &self.agent_registry,
            &self.server_config.agents.review,
            agent.name(),
        );

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
        )
        .await;

        Ok(task_id)
    }

    async fn enqueue_background(
        &self,
        mut req: CreateTaskRequest,
    ) -> Result<TaskId, EnqueueTaskError> {
        Self::validate_request(&req)?;

        req.project = self.resolve_project(req.project).await?;

        // Resolve agent up-front so validation errors surface immediately.
        let agent = self.select_agent(&req)?;
        let (reviewer, _) = resolve_reviewer(
            &self.agent_registry,
            &self.server_config.agents.review,
            agent.name(),
        );

        let canonical = task_runner::resolve_canonical_project(req.project.clone())
            .await
            .map_err(|e| EnqueueTaskError::Internal(e.to_string()))?;
        self.check_allowed_roots(&canonical)?;

        let project_id = canonical.to_string_lossy().into_owned();
        req.project = Some(canonical);
        task_runner::fill_missing_repo_from_project(&mut req).await;

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
        let task_id2 = task_id.clone();
        tokio::spawn(async move {
            match task_queue.acquire(&project_id).await {
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
        assert!(result.is_none());
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
        assert_eq!(result, Some(path));
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
            allowed,
        )
    }
}
