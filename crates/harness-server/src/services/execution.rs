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
use harness_core::{
    config::HarnessConfig, interceptor::TurnInterceptor, project_identity::ProjectIdentity,
};
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
    async fn resolve_project_identity(
        &self,
        project: Option<PathBuf>,
    ) -> Result<Option<ProjectIdentity>, EnqueueTaskError> {
        match (self.project_registry.as_ref(), project) {
            (Some(registry), Some(project_path)) => {
                if project_path.is_dir() {
                    return Ok(Some(ProjectIdentity::from_root(project_path)));
                }
                let id = project_path.to_string_lossy();
                match registry.resolve_identity(&id).await {
                    Ok(Some(identity)) => Ok(Some(identity)),
                    Ok(None) => Err(EnqueueTaskError::BadRequest(format!(
                        "project '{id}' not found in registry and is not a valid directory"
                    ))),
                    Err(e) => Err(EnqueueTaskError::Internal(e.to_string())),
                }
            }
            (_, Some(project_path)) => Ok(Some(ProjectIdentity::from_root(project_path))),
            (_, None) => Ok(None),
        }
    }

    async fn resolve_execution_identity(
        &self,
        project: Option<PathBuf>,
    ) -> Result<ProjectIdentity, EnqueueTaskError> {
        match self.resolve_project_identity(project).await? {
            Some(identity) => Ok(identity),
            None => task_runner::resolve_canonical_project(None)
                .await
                .map(ProjectIdentity::from_root)
                .map_err(|e| EnqueueTaskError::Internal(e.to_string())),
        }
    }

    fn queue_key(identity: &ProjectIdentity) -> String {
        identity.queue_key()
    }

    fn apply_execution_identity(req: &mut CreateTaskRequest, identity: &ProjectIdentity) {
        req.project = Some(identity.canonical_root().to_path_buf());
    }

    fn queue_project_id(identity: &ProjectIdentity) -> String {
        Self::queue_key(identity)
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

        let identity = self.resolve_execution_identity(req.project.clone()).await?;
        self.check_allowed_roots(identity.canonical_root())?;

        let project_id = Self::queue_project_id(&identity);
        Self::apply_execution_identity(&mut req, &identity);

        let permit = self
            .task_queue
            .acquire(&project_id, req.priority)
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

        let identity = self.resolve_execution_identity(req.project.clone()).await?;

        // Resolve agent up-front so validation errors surface immediately.
        let agent = self.select_agent(&req)?;
        let (reviewer, _) = resolve_reviewer(
            &self.agent_registry,
            &self.server_config.agents.review,
            agent.name(),
        );

        self.check_allowed_roots(identity.canonical_root())?;

        let project_id = Self::queue_project_id(&identity);
        Self::apply_execution_identity(&mut req, &identity);
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
    async fn resolve_project_identity_none_passes_through() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = TaskStore::open(&dir.path().join("t.db")).await?;
        let svc = make_minimal_svc(store, None).await;

        assert!(svc.resolve_project_identity(None).await?.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn resolve_project_identity_preserves_registry_alias() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = TaskStore::open(&dir.path().join("t.db")).await?;
        let registry = ProjectRegistry::open(&dir.path().join("p.db")).await?;
        registry
            .register(crate::project_registry::Project {
                id: "alpha".to_string(),
                root: dir.path().join("repo"),
                max_concurrent: None,
                default_agent: None,
                active: true,
                created_at: "2026-01-01T00:00:00Z".to_string(),
            })
            .await?;
        let svc = make_minimal_svc(store, Some(registry)).await;

        let identity = svc
            .resolve_project_identity(Some(PathBuf::from("alpha")))
            .await?
            .expect("registry project should resolve");
        assert_eq!(identity.alias_id(), Some("alpha"));
        assert!(identity.canonical_root().ends_with("repo"));
        Ok(())
    }

    #[tokio::test]
    async fn queue_project_id_uses_canonical_root_for_registry_alias() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let root = dir.path().join("repo");
        std::fs::create_dir_all(&root)?;

        let identity = ProjectIdentity::from_registry("alpha".to_string(), root.clone());
        assert_eq!(
            DefaultExecutionService::queue_project_id(&identity),
            root.canonicalize()?.display().to_string()
        );
        Ok(())
    }

    #[tokio::test]
    async fn apply_execution_identity_rewrites_request_to_canonical_root() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let root = dir.path().join("repo");
        std::fs::create_dir_all(&root)?;
        let identity = ProjectIdentity::from_registry("alpha".to_string(), root.clone());
        let mut req = CreateTaskRequest {
            project: Some(PathBuf::from("alpha")),
            ..Default::default()
        };

        DefaultExecutionService::apply_execution_identity(&mut req, &identity);
        assert_eq!(req.project, Some(root.canonicalize()?));
        Ok(())
    }

    #[tokio::test]
    async fn resolve_execution_identity_canonicalizes_existing_dir() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = TaskStore::open(&dir.path().join("t.db")).await?;
        let svc = make_minimal_svc(store, None).await;
        let root = dir.path().join("repo");
        std::fs::create_dir_all(&root)?;
        let link = dir.path().join("repo-link");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&root, &link)?;
        #[cfg(windows)]
        std::os::windows::fs::symlink_dir(&root, &link)?;

        let identity = svc.resolve_execution_identity(Some(link)).await?;
        assert_eq!(identity.canonical_root(), root.canonicalize()?.as_path());
        assert_eq!(identity.alias_id(), None);
        Ok(())
    }

    #[tokio::test]
    async fn resolve_execution_identity_uses_detected_worktree_when_missing() -> anyhow::Result<()>
    {
        let dir = tempfile::tempdir()?;
        let store = TaskStore::open(&dir.path().join("t.db")).await?;
        let svc = make_minimal_svc(store, None).await;

        let identity = svc.resolve_execution_identity(None).await?;
        assert!(
            identity.canonical_root().is_absolute()
                || identity.canonical_root() == PathBuf::from(".").as_path()
        );
        Ok(())
    }

    #[tokio::test]
    async fn resolve_project_identity_existing_dir_has_no_alias() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = TaskStore::open(&dir.path().join("t.db")).await?;
        let svc = make_minimal_svc(store, None).await;
        let root = dir.path().join("repo");
        std::fs::create_dir_all(&root)?;

        let identity = svc
            .resolve_project_identity(Some(root.clone()))
            .await?
            .expect("existing dir should resolve");
        assert_eq!(identity.alias_id(), None);
        assert_eq!(identity.canonical_root(), root.canonicalize()?.as_path());
        Ok(())
    }

    #[tokio::test]
    async fn resolve_project_identity_unknown_id_returns_bad_request() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = TaskStore::open(&dir.path().join("t.db")).await?;
        let registry = ProjectRegistry::open(&dir.path().join("p.db")).await?;
        let svc = make_minimal_svc(store, Some(registry)).await;

        let result = svc
            .resolve_project_identity(Some(PathBuf::from("nonexistent-id")))
            .await;
        assert!(matches!(result, Err(EnqueueTaskError::BadRequest(_))));
        Ok(())
    }

    #[tokio::test]
    async fn queue_project_id_matches_task_queue_canonical_bucket() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let root = dir.path().join("repo");
        std::fs::create_dir_all(&root)?;
        let link = dir.path().join("repo-link");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&root, &link)?;
        #[cfg(windows)]
        std::os::windows::fs::symlink_dir(&root, &link)?;

        let identity = ProjectIdentity::from_root(link);
        assert_eq!(
            DefaultExecutionService::queue_project_id(&identity),
            harness_core::project_identity::canonical_project_key(&root)
        );
        Ok(())
    }

    #[tokio::test]
    async fn apply_execution_identity_overwrites_alias_with_canonical_root() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let root = dir.path().join("repo");
        std::fs::create_dir_all(&root)?;
        let identity = ProjectIdentity::from_registry("alpha".to_string(), root.clone());
        let mut req = CreateTaskRequest {
            project: Some(PathBuf::from("different")),
            ..Default::default()
        };
        DefaultExecutionService::apply_execution_identity(&mut req, &identity);
        assert_eq!(req.project, Some(root.canonicalize()?));
        Ok(())
    }

    #[tokio::test]
    async fn queue_project_id_ignores_registry_alias_text() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let root = dir.path().join("repo");
        std::fs::create_dir_all(&root)?;
        let identity = ProjectIdentity::from_registry("some-alias".to_string(), root.clone());
        assert_ne!(
            DefaultExecutionService::queue_project_id(&identity),
            "some-alias"
        );
        Ok(())
    }

    #[tokio::test]
    async fn resolve_execution_identity_from_registry_keeps_alias_and_canonical_root(
    ) -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = TaskStore::open(&dir.path().join("t.db")).await?;
        let registry = ProjectRegistry::open(&dir.path().join("p.db")).await?;
        let root = dir.path().join("repo");
        std::fs::create_dir_all(&root)?;
        registry
            .register(crate::project_registry::Project {
                id: "alpha".to_string(),
                root: root.clone(),
                max_concurrent: None,
                default_agent: None,
                active: true,
                created_at: "2026-01-01T00:00:00Z".to_string(),
            })
            .await?;
        let svc = make_minimal_svc(store, Some(registry)).await;

        let identity = svc
            .resolve_execution_identity(Some(PathBuf::from("alpha")))
            .await?;
        assert_eq!(identity.alias_id(), Some("alpha"));
        assert_eq!(identity.canonical_root(), root.canonicalize()?.as_path());
        Ok(())
    }

    #[tokio::test]
    async fn resolve_project_identity_from_registry_canonicalizes_symlink_root(
    ) -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = TaskStore::open(&dir.path().join("t.db")).await?;
        let registry = ProjectRegistry::open(&dir.path().join("p.db")).await?;
        let root = dir.path().join("repo");
        std::fs::create_dir_all(&root)?;
        let alias_root = dir.path().join("repo-link");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&root, &alias_root)?;
        #[cfg(windows)]
        std::os::windows::fs::symlink_dir(&root, &alias_root)?;
        registry
            .register(crate::project_registry::Project {
                id: "alpha".to_string(),
                root: alias_root,
                max_concurrent: None,
                default_agent: None,
                active: true,
                created_at: "2026-01-01T00:00:00Z".to_string(),
            })
            .await?;
        let svc = make_minimal_svc(store, Some(registry)).await;

        let identity = svc
            .resolve_project_identity(Some(PathBuf::from("alpha")))
            .await?
            .expect("registry project should resolve");
        assert_eq!(identity.canonical_root(), root.canonicalize()?.as_path());
        Ok(())
    }

    #[tokio::test]
    async fn resolve_project_identity_non_registry_path_is_canonicalized() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = TaskStore::open(&dir.path().join("t.db")).await?;
        let svc = make_minimal_svc(store, None).await;
        let root = dir.path().join("repo");
        std::fs::create_dir_all(&root)?;
        let link = dir.path().join("repo-link");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&root, &link)?;
        #[cfg(windows)]
        std::os::windows::fs::symlink_dir(&root, &link)?;

        let identity = svc
            .resolve_project_identity(Some(link))
            .await?
            .expect("path should resolve");
        assert_eq!(identity.canonical_root(), root.canonicalize()?.as_path());
        Ok(())
    }

    #[tokio::test]
    async fn queue_project_id_for_existing_dir_matches_canonical_display() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let root = dir.path().join("repo");
        std::fs::create_dir_all(&root)?;
        let identity = ProjectIdentity::from_root(root.clone());
        assert_eq!(
            DefaultExecutionService::queue_project_id(&identity),
            root.canonicalize()?.display().to_string()
        );
        Ok(())
    }

    #[tokio::test]
    async fn resolve_project_identity_registry_missing_still_errors() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = TaskStore::open(&dir.path().join("t.db")).await?;
        let registry = ProjectRegistry::open(&dir.path().join("p.db")).await?;
        let svc = make_minimal_svc(store, Some(registry)).await;
        let result = svc
            .resolve_project_identity(Some(PathBuf::from("missing")))
            .await;
        assert!(matches!(result, Err(EnqueueTaskError::BadRequest(_))));
        Ok(())
    }

    #[tokio::test]
    async fn resolve_execution_identity_none_produces_queueable_identity() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = TaskStore::open(&dir.path().join("t.db")).await?;
        let svc = make_minimal_svc(store, None).await;
        let identity = svc.resolve_execution_identity(None).await?;
        assert!(!DefaultExecutionService::queue_project_id(&identity).is_empty());
        Ok(())
    }

    #[tokio::test]
    async fn apply_execution_identity_preserves_none_repo_field() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let root = dir.path().join("repo");
        std::fs::create_dir_all(&root)?;
        let identity = ProjectIdentity::from_root(root.clone());
        let mut req = CreateTaskRequest::default();
        DefaultExecutionService::apply_execution_identity(&mut req, &identity);
        assert_eq!(req.project, Some(root.canonicalize()?));
        assert!(req.repo.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn queue_project_id_for_registry_alias_equals_canonical_path_not_alias(
    ) -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let root = dir.path().join("repo");
        std::fs::create_dir_all(&root)?;
        let identity = ProjectIdentity::from_registry("alpha".to_string(), root.clone());
        let queue_key = DefaultExecutionService::queue_project_id(&identity);
        assert_eq!(queue_key, root.canonicalize()?.display().to_string());
        assert_ne!(queue_key, "alpha");
        Ok(())
    }

    #[tokio::test]
    async fn resolve_project_identity_without_registry_uses_from_root() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = TaskStore::open(&dir.path().join("t.db")).await?;
        let svc = make_minimal_svc(store, None).await;
        let root = dir.path().join("repo");
        std::fs::create_dir_all(&root)?;

        let identity = svc
            .resolve_project_identity(Some(root.clone()))
            .await?
            .expect("dir should resolve");
        assert_eq!(identity, ProjectIdentity::from_root(root));
        Ok(())
    }

    #[tokio::test]
    async fn resolve_project_identity_registry_path_round_trip() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = TaskStore::open(&dir.path().join("t.db")).await?;
        let registry = ProjectRegistry::open(&dir.path().join("p.db")).await?;
        let root = dir.path().join("repo");
        std::fs::create_dir_all(&root)?;
        registry
            .register(crate::project_registry::Project {
                id: "alpha".to_string(),
                root: root.clone(),
                max_concurrent: None,
                default_agent: None,
                active: true,
                created_at: "2026-01-01T00:00:00Z".to_string(),
            })
            .await?;
        let svc = make_minimal_svc(store, Some(registry)).await;

        let identity = svc
            .resolve_project_identity(Some(PathBuf::from("alpha")))
            .await?
            .expect("should resolve");
        let mut req = CreateTaskRequest {
            project: Some(PathBuf::from("alpha")),
            ..Default::default()
        };
        DefaultExecutionService::apply_execution_identity(&mut req, &identity);
        assert_eq!(req.project, Some(root.canonicalize()?));
        Ok(())
    }

    #[tokio::test]
    async fn resolve_project_identity_prefers_registry_for_non_dir_input() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = TaskStore::open(&dir.path().join("t.db")).await?;
        let registry = ProjectRegistry::open(&dir.path().join("p.db")).await?;
        let root = dir.path().join("repo");
        std::fs::create_dir_all(&root)?;
        registry
            .register(crate::project_registry::Project {
                id: "alpha".to_string(),
                root: root.clone(),
                max_concurrent: None,
                default_agent: None,
                active: true,
                created_at: "2026-01-01T00:00:00Z".to_string(),
            })
            .await?;
        let svc = make_minimal_svc(store, Some(registry)).await;
        let identity = svc
            .resolve_project_identity(Some(PathBuf::from("alpha")))
            .await?
            .expect("registry should win");
        assert_eq!(identity.alias_id(), Some("alpha"));
        Ok(())
    }

    #[tokio::test]
    async fn resolve_project_identity_existing_dir_does_not_require_registry() -> anyhow::Result<()>
    {
        let dir = tempfile::tempdir()?;
        let store = TaskStore::open(&dir.path().join("t.db")).await?;
        let registry = ProjectRegistry::open(&dir.path().join("p.db")).await?;
        let svc = make_minimal_svc(store, Some(registry)).await;
        let root = dir.path().join("repo");
        std::fs::create_dir_all(&root)?;
        let identity = svc
            .resolve_project_identity(Some(root.clone()))
            .await?
            .expect("dir should resolve");
        assert_eq!(identity.alias_id(), None);
        assert_eq!(identity.canonical_root(), root.canonicalize()?.as_path());
        Ok(())
    }

    #[tokio::test]
    async fn queue_project_id_is_stable_for_canonical_root() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let root = dir.path().join("repo");
        std::fs::create_dir_all(&root)?;
        let identity = ProjectIdentity::from_root(root.clone());
        let a = DefaultExecutionService::queue_project_id(&identity);
        let b = DefaultExecutionService::queue_project_id(&identity);
        assert_eq!(a, b);
        Ok(())
    }

    #[tokio::test]
    async fn resolve_project_identity_registry_and_path_converge_to_same_queue_key(
    ) -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store = TaskStore::open(&dir.path().join("t.db")).await?;
        let registry = ProjectRegistry::open(&dir.path().join("p.db")).await?;
        let root = dir.path().join("repo");
        std::fs::create_dir_all(&root)?;
        registry
            .register(crate::project_registry::Project {
                id: "alpha".to_string(),
                root: root.clone(),
                max_concurrent: None,
                default_agent: None,
                active: true,
                created_at: "2026-01-01T00:00:00Z".to_string(),
            })
            .await?;
        let svc = make_minimal_svc(store, Some(registry)).await;
        let by_id = svc
            .resolve_project_identity(Some(PathBuf::from("alpha")))
            .await?
            .expect("id should resolve");
        let by_path = svc
            .resolve_project_identity(Some(root.clone()))
            .await?
            .expect("path should resolve");
        assert_eq!(
            DefaultExecutionService::queue_project_id(&by_id),
            DefaultExecutionService::queue_project_id(&by_path)
        );
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
