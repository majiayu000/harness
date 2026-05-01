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
use harness_workflow::issue_lifecycle::{
    is_feedback_claim_placeholder, IssueLifecycleState, IssueWorkflowInstance,
};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{RwLock, Semaphore};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueueDomain {
    Primary,
    Review,
}

impl QueueDomain {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Primary => "primary",
            Self::Review => "review",
        }
    }
}

pub struct EnqueueBackgroundOptions {
    pub(crate) queue_domain: QueueDomain,
    pub(crate) group_sem: Option<Arc<Semaphore>>,
}

impl Default for EnqueueBackgroundOptions {
    fn default() -> Self {
        Self {
            queue_domain: QueueDomain::Primary,
            group_sem: None,
        }
    }
}

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

    /// Enqueue a task in a specific queue domain.
    async fn enqueue_in_domain(
        &self,
        req: CreateTaskRequest,
        queue_domain: QueueDomain,
    ) -> Result<TaskId, EnqueueTaskError>;

    /// Register a task immediately and begin execution in the background.
    ///
    /// Returns the task ID without waiting for a concurrency permit. A
    /// background tokio task handles permit acquisition and execution, keeping
    /// HTTP handlers responsive under load.
    async fn enqueue_background(&self, req: CreateTaskRequest) -> Result<TaskId, EnqueueTaskError>;

    /// Register a task immediately with background execution options.
    async fn enqueue_background_with_options(
        &self,
        req: CreateTaskRequest,
        options: EnqueueBackgroundOptions,
    ) -> Result<TaskId, EnqueueTaskError>;
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
    review_task_queue: Arc<TaskQueue>,
    completion_callback: Option<CompletionCallback>,
    issue_workflow_store: Option<Arc<harness_workflow::issue_lifecycle::IssueWorkflowStore>>,
    workflow_runtime_store: Option<Arc<harness_workflow::runtime::WorkflowRuntimeStore>>,
    project_registry: Option<Arc<ProjectRegistry>>,
    allowed_project_roots: Vec<PathBuf>,
}

struct PreparedEnqueue {
    req: CreateTaskRequest,
    project_id: String,
    agent: Arc<dyn CodeAgent>,
    reviewer: Option<Arc<dyn CodeAgent>>,
}

struct PreparedRuntimeIssueSubmission {
    req: CreateTaskRequest,
    project_id: String,
}

const WORKFLOW_FEEDBACK_SOURCE: &str = "workflow_feedback";

enum PreparedEnqueueResult {
    Existing(TaskId),
    RuntimeIssueSubmission(Box<PreparedRuntimeIssueSubmission>),
    Ready(Box<PreparedEnqueue>),
}

fn is_workflow_feedback_request(req: &CreateTaskRequest) -> bool {
    req.pr.is_some() && req.source.as_deref() == Some(WORKFLOW_FEEDBACK_SOURCE)
}

fn allows_terminal_pr_duplicate_reuse(req: &CreateTaskRequest) -> bool {
    !is_workflow_feedback_request(req)
}

pub(crate) enum WorkflowReuseStrategy {
    ActiveTask(TaskId),
    ActivePrExternalId(String),
    None,
}

pub(crate) fn workflow_state_allows_reuse(state: IssueLifecycleState) -> bool {
    !matches!(
        state,
        IssueLifecycleState::Failed | IssueLifecycleState::Cancelled
    )
}

pub(crate) fn workflow_reuse_strategy(workflow: &IssueWorkflowInstance) -> WorkflowReuseStrategy {
    if !workflow_state_allows_reuse(workflow.state) {
        return WorkflowReuseStrategy::None;
    }
    if let Some(task_id) = workflow.active_task_id.as_ref() {
        if !is_feedback_claim_placeholder(task_id) {
            return WorkflowReuseStrategy::ActiveTask(harness_core::types::TaskId(task_id.clone()));
        }
    }
    if let Some(pr_number) = workflow.pr_number {
        return WorkflowReuseStrategy::ActivePrExternalId(format!("pr:{pr_number}"));
    }
    WorkflowReuseStrategy::None
}

pub(crate) async fn resolve_project_from_registry(
    registry: Option<&ProjectRegistry>,
    project: Option<PathBuf>,
) -> Result<(Option<PathBuf>, Option<String>), EnqueueTaskError> {
    let (Some(registry), Some(project_path)) = (registry, project.clone()) else {
        return Ok((project, None));
    };

    if tokio::fs::metadata(&project_path)
        .await
        .map(|metadata| metadata.is_dir())
        .unwrap_or(false)
    {
        let canonical_root = tokio::fs::canonicalize(&project_path)
            .await
            .unwrap_or(project_path);
        return match registry.get_by_root(&canonical_root).await {
            Ok(Some(project)) => Ok((Some(project.root), project.default_agent)),
            Ok(None) => Ok((Some(canonical_root), None)),
            Err(e) => Err(EnqueueTaskError::Internal(e.to_string())),
        };
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

pub(crate) fn queue_timeout_message(
    queue: &TaskQueue,
    project_id: &str,
    domain: QueueDomain,
) -> String {
    let diag = queue.diagnostics(project_id);
    let project_holding = diag.project_running + diag.project_awaiting_global;
    let project_full = project_holding >= diag.project_limit;
    let global_full = diag.global_running >= diag.global_limit;
    let reason = match (global_full, project_full, domain) {
        (_, _, QueueDomain::Review) => "review capacity domain full",
        (true, true, _) => "global and project capacity saturated",
        (true, false, _) => "global capacity saturated",
        (false, true, _) => "project capacity saturated",
        (false, false, _) => "permit wait exceeded timeout",
    };
    format!(
        "{reason} (domain={}, global_running={}, global_queued={}, global_limit={}, project_running={}, project_waiting={}, project_awaiting_global={}, project_limit={})",
        domain.label(),
        diag.global_running,
        diag.global_queued,
        diag.global_limit,
        diag.project_running,
        diag.project_waiting_for_project,
        diag.project_awaiting_global,
        diag.project_limit,
    )
}

pub(crate) fn select_agent(
    req: &CreateTaskRequest,
    registry: &AgentRegistry,
    registry_agent: Option<&str>,
) -> Result<Arc<dyn CodeAgent>, EnqueueTaskError> {
    if let Some(name) = &req.agent {
        return registry
            .get(name)
            .ok_or_else(|| EnqueueTaskError::BadRequest(format!("agent '{name}' not registered")));
    }

    if let Some(project_root) = &req.project {
        let project_cfg = harness_core::config::project::load_project_config(project_root)
            .map_err(|e| EnqueueTaskError::Internal(e.to_string()))?;
        if let Some(agent_name) = project_cfg.agent.as_ref().and_then(|a| a.default.as_ref()) {
            if agent_name != "auto" {
                return registry.get(agent_name).ok_or_else(|| {
                    EnqueueTaskError::BadRequest(format!("agent '{agent_name}' not registered"))
                });
            }
        }
    }

    if let Some(name) = registry_agent {
        if name != "auto" {
            return registry.get(name).ok_or_else(|| {
                EnqueueTaskError::BadRequest(format!("agent '{name}' not registered"))
            });
        }
    }

    let classification =
        complexity_router::classify(req.prompt.as_deref().unwrap_or_default(), req.issue, req.pr);
    registry
        .dispatch(&classification)
        .map_err(|e| EnqueueTaskError::Internal(e.to_string()))
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
        review_task_queue: Arc<TaskQueue>,
        completion_callback: Option<CompletionCallback>,
        issue_workflow_store: Option<Arc<harness_workflow::issue_lifecycle::IssueWorkflowStore>>,
        workflow_runtime_store: Option<Arc<harness_workflow::runtime::WorkflowRuntimeStore>>,
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
            review_task_queue,
            completion_callback,
            issue_workflow_store,
            workflow_runtime_store,
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
        resolve_project_from_registry(self.project_registry.as_deref(), project).await
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

    fn queue_for(&self, domain: QueueDomain) -> Arc<TaskQueue> {
        match domain {
            QueueDomain::Primary => self.task_queue.clone(),
            QueueDomain::Review => self.review_task_queue.clone(),
        }
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

        self.lookup_workflow_reuse_candidate(project_id, &workflow)
            .await
    }

    async fn lookup_workflow_reuse_candidate(
        &self,
        project_id: &str,
        workflow: &IssueWorkflowInstance,
    ) -> Option<TaskId> {
        match workflow_reuse_strategy(workflow) {
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
            WorkflowReuseStrategy::ActivePrExternalId(pr_ext_id) => {
                return self
                    .tasks
                    .find_active_duplicate(project_id, &pr_ext_id)
                    .await;
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
        if !allows_terminal_pr_duplicate_reuse(req) {
            return None;
        }
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

    async fn dependencies_blocked(
        &self,
        req: &CreateTaskRequest,
    ) -> Result<bool, EnqueueTaskError> {
        if req.depends_on.is_empty() {
            return Ok(false);
        }
        for dep_id in &req.depends_on {
            let status = crate::workflow_runtime_submission::resolve_issue_dependency_status(
                self.workflow_runtime_store.as_deref(),
                &self.tasks,
                dep_id,
            )
            .await
            .map_err(|error| {
                EnqueueTaskError::Internal(format!(
                    "dependency status lookup failed for {}: {error}",
                    dep_id.as_str()
                ))
            })?;
            if !matches!(
                status,
                crate::workflow_runtime_submission::IssueDependencyStatus::Done
            ) {
                return Ok(true);
            }
        }
        Ok(false)
    }

    async fn check_runtime_issue_duplicate(
        &self,
        project_id: &str,
        req: &CreateTaskRequest,
    ) -> Result<Option<TaskId>, EnqueueTaskError> {
        let Some(issue_number) = req.issue else {
            return Ok(None);
        };
        let Some(store) = self.workflow_runtime_store.as_ref() else {
            return Ok(None);
        };
        let workflow_id = harness_workflow::issue_lifecycle::workflow_id(
            project_id,
            req.repo.as_deref(),
            issue_number,
        );
        let Some(instance) = store
            .get_instance(&workflow_id)
            .await
            .map_err(|error| EnqueueTaskError::Internal(error.to_string()))?
        else {
            return Ok(None);
        };
        if instance.is_terminal() {
            return Ok(None);
        }
        Ok(crate::workflow_runtime_submission::runtime_issue_task_handle(&instance))
    }

    async fn track_pr_workflow(&self, project_id: &str, req: &CreateTaskRequest, task_id: &TaskId) {
        if let Some(pr_number) = req.pr {
            if let Some(workflows) = self.issue_workflow_store.as_ref() {
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

    async fn submit_issue_to_workflow_runtime(
        &self,
        prepared: PreparedRuntimeIssueSubmission,
    ) -> Result<TaskId, EnqueueTaskError> {
        let Some(store) = self.workflow_runtime_store.as_deref() else {
            return Err(EnqueueTaskError::Internal(
                "workflow runtime store is required for GitHub issue submissions".to_string(),
            ));
        };
        let Some(issue_number) = prepared.req.issue else {
            return Err(EnqueueTaskError::BadRequest(
                "issue submission requires an issue number".to_string(),
            ));
        };
        let dependencies_blocked = self.dependencies_blocked(&prepared.req).await?;

        let task_id = TaskId::new();
        let project_root = prepared
            .req
            .project
            .as_deref()
            .unwrap_or_else(|| std::path::Path::new(&prepared.project_id));
        let record = crate::workflow_runtime_submission::record_issue_submission(
            store,
            crate::workflow_runtime_submission::IssueSubmissionRuntimeContext {
                project_root,
                repo: prepared.req.repo.as_deref(),
                issue_number,
                task_id: &task_id,
                labels: &prepared.req.labels,
                force_execute: prepared.req.force_execute,
                additional_prompt: prepared.req.prompt.as_deref(),
                depends_on: &prepared.req.depends_on,
                dependencies_blocked,
            },
        )
        .await
        .map_err(|error| EnqueueTaskError::Internal(error.to_string()))?;

        if !record.accepted {
            return Err(EnqueueTaskError::BadRequest(format!(
                "workflow runtime rejected issue submission for workflow {}: {}",
                record.workflow_id,
                record
                    .rejection_reason
                    .as_deref()
                    .unwrap_or("decision rejected")
            )));
        }
        if record.command_ids.is_empty() && !dependencies_blocked {
            return Err(EnqueueTaskError::Internal(format!(
                "workflow runtime accepted issue submission for workflow {} without commands",
                record.workflow_id
            )));
        }
        tracing::info!(
            workflow_id = %record.workflow_id,
            task_id = %task_id.0,
            command_count = record.command_ids.len(),
            "execution service: accepted issue submission into workflow runtime"
        );
        Ok(task_id)
    }

    async fn prepare_enqueue(
        &self,
        mut req: CreateTaskRequest,
    ) -> Result<PreparedEnqueueResult, EnqueueTaskError> {
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

        if req.issue.is_some() && self.workflow_runtime_store.is_some() {
            if let Some(existing_id) = self
                .check_runtime_issue_duplicate(&project_id, &req)
                .await?
            {
                return Ok(PreparedEnqueueResult::Existing(existing_id));
            }
            return Ok(PreparedEnqueueResult::RuntimeIssueSubmission(Box::new(
                PreparedRuntimeIssueSubmission { req, project_id },
            )));
        }

        if let Some(existing_id) = self.check_workflow_duplicate(&project_id, &req).await {
            return Ok(PreparedEnqueueResult::Existing(existing_id));
        }
        if let Some(existing_id) = self.check_duplicate(&project_id, &req).await {
            return Ok(PreparedEnqueueResult::Existing(existing_id));
        }
        if let Some(existing_id) = self.check_pr_duplicate(&project_id, &req).await {
            return Ok(PreparedEnqueueResult::Existing(existing_id));
        }

        if self.dependencies_blocked(&req).await? {
            let workflow_req = req.clone();
            let task_id = task_runner::spawn_task_awaiting_deps(self.tasks.clone(), req)
                .await
                .map_err(|e| EnqueueTaskError::BadRequest(e.to_string()))?;
            self.track_pr_workflow(&project_id, &workflow_req, &task_id)
                .await;
            return Ok(PreparedEnqueueResult::Existing(task_id));
        }
        req.depends_on.clear();

        let agent = select_agent(
            &req,
            &self.agent_registry,
            registry_default_agent.as_deref(),
        )?;
        let (reviewer, _) = resolve_reviewer(
            &self.agent_registry,
            &self.server_config.agents.review,
            agent.name(),
        );

        Ok(PreparedEnqueueResult::Ready(Box::new(PreparedEnqueue {
            req,
            project_id,
            agent,
            reviewer,
        })))
    }
}

#[async_trait]
impl ExecutionService for DefaultExecutionService {
    async fn enqueue(&self, req: CreateTaskRequest) -> Result<TaskId, EnqueueTaskError> {
        self.enqueue_in_domain(req, QueueDomain::Primary).await
    }

    async fn enqueue_in_domain(
        &self,
        req: CreateTaskRequest,
        queue_domain: QueueDomain,
    ) -> Result<TaskId, EnqueueTaskError> {
        let prepared = match self.prepare_enqueue(req).await? {
            PreparedEnqueueResult::Existing(task_id) => return Ok(task_id),
            PreparedEnqueueResult::RuntimeIssueSubmission(prepared) => {
                return self.submit_issue_to_workflow_runtime(*prepared).await;
            }
            PreparedEnqueueResult::Ready(prepared) => *prepared,
        };

        let queue = self.queue_for(queue_domain);
        let permit = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            queue.acquire(&prepared.project_id, prepared.req.priority),
        )
        .await
        .map_err(|_| {
            EnqueueTaskError::Internal(queue_timeout_message(
                &queue,
                &prepared.project_id,
                queue_domain,
            ))
        })?
        .map_err(|e| EnqueueTaskError::Internal(e.to_string()))?;

        let workflow_req = prepared.req.clone();
        let task_id = task_runner::spawn_task(
            self.tasks.clone(),
            prepared.agent,
            prepared.reviewer,
            self.server_config.clone(),
            self.skills.clone(),
            self.events.clone(),
            self.interceptors.clone(),
            prepared.req,
            self.workspace_mgr.clone(),
            permit,
            self.completion_callback.clone(),
            self.issue_workflow_store.clone(),
            self.workflow_runtime_store.clone(),
            self.allowed_project_roots.clone(),
        )
        .await;

        self.track_pr_workflow(&prepared.project_id, &workflow_req, &task_id)
            .await;

        Ok(task_id)
    }

    async fn enqueue_background(&self, req: CreateTaskRequest) -> Result<TaskId, EnqueueTaskError> {
        self.enqueue_background_with_options(req, EnqueueBackgroundOptions::default())
            .await
    }

    async fn enqueue_background_with_options(
        &self,
        req: CreateTaskRequest,
        options: EnqueueBackgroundOptions,
    ) -> Result<TaskId, EnqueueTaskError> {
        let prepared = match self.prepare_enqueue(req).await? {
            PreparedEnqueueResult::Existing(task_id) => return Ok(task_id),
            PreparedEnqueueResult::RuntimeIssueSubmission(prepared) => {
                return self.submit_issue_to_workflow_runtime(*prepared).await;
            }
            PreparedEnqueueResult::Ready(prepared) => *prepared,
        };

        let server_config = self.server_config.clone();

        tracing::info!(
            project = %prepared.req.project.as_ref().map(|p| p.display().to_string()).unwrap_or_else(|| "None".to_string()),
            domain = options.queue_domain.label(),
            "execution service: resolved project for background task"
        );

        // Register the task immediately so the caller gets an ID without blocking.
        let task_id = task_runner::register_pending_task(self.tasks.clone(), &prepared.req).await;

        // Spawn a background tokio task that waits for a concurrency slot then executes.
        let tasks = self.tasks.clone();
        let skills = self.skills.clone();
        let events = self.events.clone();
        let interceptors = self.interceptors.clone();
        let workspace_mgr = self.workspace_mgr.clone();
        let queue_domain = options.queue_domain;
        let task_queue = self.queue_for(queue_domain);
        let group_sem = options.group_sem;
        let completion_callback = self.completion_callback.clone();
        let issue_workflow_store = self.issue_workflow_store.clone();
        let workflow_runtime_store = self.workflow_runtime_store.clone();
        let allowed_project_roots = self.allowed_project_roots.clone();
        let task_id2 = task_id.clone();
        let project_id = prepared.project_id;
        self.track_pr_workflow(&project_id, &prepared.req, &task_id)
            .await;
        let req = prepared.req;
        let agent = prepared.agent;
        let reviewer = prepared.reviewer;
        tokio::spawn(async move {
            let group_permit = if let Some(sem) = group_sem {
                sem.acquire_owned().await.ok()
            } else {
                None
            };
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
                        workflow_runtime_store,
                        allowed_project_roots,
                        group_permit,
                    )
                    .await;
                }
                Err(e) => {
                    let queue_error =
                        format!("{} queue admission failed: {e}", queue_domain.label());
                    tracing::error!(
                        task_id = %task_id2.0,
                        project = %project_id,
                        domain = queue_domain.label(),
                        error = %queue_error,
                        "background task admission failed"
                    );
                    if let Err(persist_err) =
                        task_runner::mutate_and_persist(&tasks, &task_id2, |s| {
                            s.status = task_runner::TaskStatus::Failed;
                            s.scheduler.mark_terminal(&task_runner::TaskStatus::Failed);
                            s.error = Some(queue_error.clone());
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
    async fn enqueue_background_issue_submission_falls_back_without_runtime_store(
    ) -> anyhow::Result<()> {
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

        let task_id = svc.enqueue_background(req).await?;
        let task = task_store
            .get_with_db_fallback(&task_id)
            .await?
            .expect("issue submission should fall back to a task runner row");
        let canonical_project_root = project_root.canonicalize()?;

        assert_eq!(task.id, task_id);
        assert_eq!(task.task_kind, task_runner::TaskKind::Issue);
        assert_eq!(task.external_id.as_deref(), Some("issue:42"));
        assert_eq!(task.repo.as_deref(), Some("owner/repo"));
        assert_eq!(
            task.project_root.as_deref(),
            Some(canonical_project_root.as_path())
        );
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
        assert_eq!(instance.state, "scheduled");
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
    async fn enqueue_background_issue_submission_reuses_active_runtime_handle() -> anyhow::Result<()>
    {
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
    async fn enqueue_background_issue_submission_preserves_runtime_dependencies(
    ) -> anyhow::Result<()> {
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
        assert_eq!(workflow.state, "scheduled");
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
}
