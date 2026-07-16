//! ExecutionService — task enqueue with project resolution, agent dispatch,
//! workspace allocation, concurrency management, and completion callbacks.

use crate::{
    complexity_router,
    http::resolve_reviewer,
    project_registry::{check_allowed_roots, ProjectRegistry},
    task_executor::SharedTurnInterceptors,
    task_queue::TaskQueue,
    task_runner::{self, CompletionCallback, CreateTaskRequest, TaskId, TaskStore},
    workspace::WorkspaceManager,
};
use async_trait::async_trait;
use harness_agents::registry::AgentRegistry;
use harness_core::{agent::CodeAgent, config::HarnessConfig};
use harness_skills::store::SkillStore;
use harness_workflow::issue_lifecycle::{
    is_feedback_claim_placeholder, IssueLifecycleState, IssueWorkflowInstance,
};
use harness_workflow::runtime::{WorkflowRuntimeStore, GITHUB_ISSUE_PR_DEFINITION_ID};
use std::path::{Path, PathBuf};
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
    interceptors: SharedTurnInterceptors,
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

struct PreparedRuntimePromptSubmission {
    req: CreateTaskRequest,
    project_id: String,
}

struct PreparedRuntimeDeclarativeSubmission {
    req: CreateTaskRequest,
    project_id: String,
}

struct PreparedRuntimePrFeedbackSubmission {
    req: CreateTaskRequest,
    project_id: String,
}

const WORKFLOW_FEEDBACK_SOURCE: &str = "workflow_feedback";

enum PreparedEnqueueResult {
    Existing(TaskId),
    RuntimeIssueSubmission(Box<PreparedRuntimeIssueSubmission>),
    RuntimePromptSubmission(Box<PreparedRuntimePromptSubmission>),
    RuntimeDeclarativeSubmission(Box<PreparedRuntimeDeclarativeSubmission>),
    RuntimePrFeedbackSubmission(Box<PreparedRuntimePrFeedbackSubmission>),
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
        interceptors: impl Into<SharedTurnInterceptors>,
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
            interceptors: interceptors.into(),
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
        if req.definition_id.is_none()
            && req.prompt.is_none()
            && req.issue.is_none()
            && req.pr.is_none()
        {
            return Err(EnqueueTaskError::BadRequest(
                "at least one of definition_id, prompt, issue, or pr must be provided".to_string(),
            ));
        }
        if let Some(definition_id) = req.definition_id.as_deref() {
            if definition_id.trim().is_empty() {
                return Err(EnqueueTaskError::BadRequest(
                    "definition_id must not be empty".to_string(),
                ));
            }
            if req
                .prompt
                .as_deref()
                .is_none_or(|prompt| prompt.trim().is_empty())
            {
                return Err(EnqueueTaskError::BadRequest(
                    "declarative workflow submissions require a non-empty prompt".to_string(),
                ));
            }
            if req.issue.is_some() || req.pr.is_some() {
                return Err(EnqueueTaskError::BadRequest(
                    "declarative workflow submissions cannot include issue or pr".to_string(),
                ));
            }
            if !req.depends_on.is_empty() || !req.serialization_depends_on.is_empty() {
                return Err(EnqueueTaskError::BadRequest(
                    "declarative workflow submissions do not support dependencies".to_string(),
                ));
            }
            if req.continuation.is_some() {
                return Err(EnqueueTaskError::BadRequest(
                    "declarative workflow submissions do not support continuation".to_string(),
                ));
            }
        }
        if req.priority > task_runner::MAX_TASK_PRIORITY {
            return Err(EnqueueTaskError::BadRequest(format!(
                "priority {} out of range; maximum is {} (0=normal, 1=high, 2=critical)",
                req.priority,
                task_runner::MAX_TASK_PRIORITY,
            )));
        }
        if let Some(policy) = req.continuation.as_ref() {
            if req.issue.is_some() || req.pr.is_some() || req.prompt.is_none() {
                return Err(EnqueueTaskError::BadRequest(
                    "continuation is only supported for prompt-only tasks".to_string(),
                ));
            }
            policy
                .validate()
                .map_err(|error| EnqueueTaskError::BadRequest(error.to_string()))?;
        }
        Ok(())
    }

    /// Check the resolved canonical project against `allowed_project_roots`.
    fn check_allowed_roots(&self, canonical: &std::path::Path) -> Result<(), EnqueueTaskError> {
        check_allowed_roots(canonical, &self.allowed_project_roots)
            .map_err(EnqueueTaskError::BadRequest)
    }

    fn runtime_issue_submission_enabled(
        &self,
        project_root: &Path,
    ) -> Result<bool, EnqueueTaskError> {
        workflow_runtime_loops_enabled(project_root)
    }

    fn runtime_prompt_submission_enabled(
        &self,
        project_root: &Path,
    ) -> Result<bool, EnqueueTaskError> {
        workflow_runtime_loops_enabled(project_root)
    }

    fn runtime_pr_feedback_enabled(&self, project_root: &Path) -> Result<bool, EnqueueTaskError> {
        workflow_runtime_loops_enabled(project_root)
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
                crate::workflow_runtime_submission::RuntimeDependencyStatus::Done
            ) {
                return Ok(true);
            }
        }
        Ok(false)
    }

    async fn serialization_dependencies_blocked(
        &self,
        req: &CreateTaskRequest,
    ) -> Result<bool, EnqueueTaskError> {
        if req.serialization_depends_on.is_empty() {
            return Ok(false);
        }
        for dep_id in &req.serialization_depends_on {
            let status = crate::workflow_runtime_submission::resolve_issue_dependency_status(
                self.workflow_runtime_store.as_deref(),
                &self.tasks,
                dep_id,
            )
            .await
            .map_err(|error| {
                EnqueueTaskError::Internal(format!(
                    "serialization dependency status lookup failed for {}: {error}",
                    dep_id.as_str()
                ))
            })?;
            if matches!(
                status,
                crate::workflow_runtime_submission::RuntimeDependencyStatus::Waiting
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

    async fn check_runtime_prompt_duplicate(
        &self,
        project_id: &str,
        req: &CreateTaskRequest,
    ) -> Result<Option<TaskId>, EnqueueTaskError> {
        let Some(external_id) = req.external_id.as_deref() else {
            return Ok(None);
        };
        let Some(store) = self.workflow_runtime_store.as_ref() else {
            return Ok(None);
        };
        let probe_task_id = TaskId::from_str(external_id);
        let workflow_id = crate::workflow_runtime_submission::prompt_workflow_id(
            project_id,
            Some(external_id),
            &probe_task_id,
        );
        let Some(instance) = store
            .get_instance(&workflow_id)
            .await
            .map_err(|error| EnqueueTaskError::Internal(error.to_string()))?
        else {
            return Ok(None);
        };
        if runtime_prompt_duplicate_allows_resubmission(&instance) {
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
        let dependencies_blocked = self.dependencies_blocked(&prepared.req).await?
            || self
                .serialization_dependencies_blocked(&prepared.req)
                .await?;

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
                source: prepared.req.source.as_deref(),
                external_id: prepared.req.external_id.as_deref(),
                remote_fact_hash: None,
                author_trust_class: None,
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
        runtime_submission_response_handle(store, &record.workflow_id, &task_id).await
    }

    async fn submit_prompt_to_workflow_runtime(
        &self,
        prepared: PreparedRuntimePromptSubmission,
    ) -> Result<TaskId, EnqueueTaskError> {
        let Some(store) = self.workflow_runtime_store.as_deref() else {
            return Err(EnqueueTaskError::Internal(
                "workflow runtime store is required for prompt submissions".to_string(),
            ));
        };
        let Some(prompt) = prepared.req.prompt.as_deref() else {
            return Err(EnqueueTaskError::BadRequest(
                "prompt submission requires a prompt".to_string(),
            ));
        };
        let dependencies_blocked = self.dependencies_blocked(&prepared.req).await?;

        let task_id = TaskId::new();
        let project_root = prepared
            .req
            .project
            .as_deref()
            .unwrap_or_else(|| std::path::Path::new(&prepared.project_id));
        let record = crate::workflow_runtime_submission::record_prompt_submission(
            store,
            crate::workflow_runtime_submission::PromptSubmissionRuntimeContext {
                project_root,
                task_id: &task_id,
                prompt,
                depends_on: &prepared.req.depends_on,
                serialization_depends_on: &prepared.req.serialization_depends_on,
                dependencies_blocked,
                source: prepared.req.source.as_deref(),
                external_id: prepared.req.external_id.as_deref(),
                continuation: prepared.req.continuation.as_ref(),
            },
        )
        .await
        .map_err(|error| EnqueueTaskError::Internal(error.to_string()))?;

        if !record.accepted {
            return Err(EnqueueTaskError::BadRequest(format!(
                "workflow runtime rejected prompt submission for workflow {}: {}",
                record.workflow_id,
                record
                    .rejection_reason
                    .as_deref()
                    .unwrap_or("decision rejected")
            )));
        }
        if record.command_ids.is_empty() && !dependencies_blocked {
            return Err(EnqueueTaskError::Internal(format!(
                "workflow runtime accepted prompt submission for workflow {} without commands",
                record.workflow_id
            )));
        }
        tracing::info!(
            workflow_id = %record.workflow_id,
            task_id = %task_id.0,
            command_count = record.command_ids.len(),
            "execution service: accepted prompt submission into workflow runtime"
        );
        runtime_submission_response_handle(store, &record.workflow_id, &task_id).await
    }

    async fn submit_declarative_to_workflow_runtime(
        &self,
        prepared: PreparedRuntimeDeclarativeSubmission,
    ) -> Result<TaskId, EnqueueTaskError> {
        let Some(store) = self.workflow_runtime_store.as_deref() else {
            return Err(EnqueueTaskError::Internal(
                "workflow runtime store is required for declarative workflow submissions"
                    .to_string(),
            ));
        };
        let Some(definition_id) = prepared.req.definition_id.as_deref() else {
            return Err(EnqueueTaskError::BadRequest(
                "declarative workflow submission requires definition_id".to_string(),
            ));
        };
        let task_id = TaskId::new();
        let project_root = prepared
            .req
            .project
            .as_deref()
            .unwrap_or_else(|| std::path::Path::new(&prepared.project_id));
        let record = crate::workflow_runtime_submission::record_declarative_submission(
            store,
            crate::workflow_runtime_submission::DeclarativeSubmissionRuntimeContext {
                project_root,
                definition_id,
                task_id: &task_id,
                prompt: prepared.req.prompt.as_deref().ok_or_else(|| {
                    EnqueueTaskError::BadRequest(
                        "declarative workflow submissions require a non-empty prompt".to_string(),
                    )
                })?,
                depends_on: &prepared.req.depends_on,
                serialization_depends_on: &prepared.req.serialization_depends_on,
                source: prepared.req.source.as_deref(),
                external_id: prepared.req.external_id.as_deref(),
            },
        )
        .await
        .map_err(|error| EnqueueTaskError::Internal(error.to_string()))?;
        if !record.accepted {
            return Err(EnqueueTaskError::BadRequest(format!(
                "workflow runtime rejected declarative submission for workflow {}: {}",
                record.workflow_id,
                record
                    .rejection_reason
                    .as_deref()
                    .unwrap_or("decision rejected")
            )));
        }
        if record.command_ids.is_empty() {
            return Err(EnqueueTaskError::Internal(format!(
                "workflow runtime accepted declarative submission for workflow {} without commands",
                record.workflow_id
            )));
        }
        runtime_submission_response_handle(store, &record.workflow_id, &task_id).await
    }

    async fn submit_pr_feedback_to_workflow_runtime(
        &self,
        prepared: PreparedRuntimePrFeedbackSubmission,
    ) -> Result<TaskId, EnqueueTaskError> {
        let Some(store) = self.workflow_runtime_store.as_deref() else {
            return Err(EnqueueTaskError::Internal(
                "workflow runtime store is required for PR feedback submissions".to_string(),
            ));
        };
        let Some(pr_number) = prepared.req.pr else {
            return Err(EnqueueTaskError::BadRequest(
                "PR feedback submission requires a PR number".to_string(),
            ));
        };

        let maybe_instance = self
            .find_runtime_issue_workflow_by_pr(
                store,
                &prepared.project_id,
                prepared.req.repo.as_deref(),
                pr_number,
            )
            .await?;

        let outcome = if let Some(instance) = maybe_instance {
            if instance.state == "pr_open" {
                crate::workflow_runtime_pr_feedback::request_local_review(store, &instance.id).await
            } else {
                crate::workflow_runtime_pr_feedback::request_pr_feedback_sweep(store, &instance.id)
                    .await
            }
        } else {
            let task_id = crate::workflow_runtime_pr_feedback::synthesized_pr_feedback_task_id(
                &prepared.project_id,
                prepared.req.repo.as_deref(),
                pr_number,
            );
            crate::workflow_runtime_pr_feedback::request_pr_feedback_sweep_for_pr(
                store,
                crate::workflow_runtime_pr_feedback::PrFeedbackSweepRuntimeContext {
                    project_root: std::path::Path::new(&prepared.project_id),
                    repo: prepared.req.repo.as_deref(),
                    task_id: &task_id,
                    pr_number,
                    pr_url: None,
                },
            )
            .await
        }
        .map_err(|error| EnqueueTaskError::Internal(error.to_string()))?;

        match outcome {
            crate::workflow_runtime_pr_feedback::PrFeedbackSweepRequestOutcome::Requested {
                workflow_id,
                task_id,
                ..
            }
            | crate::workflow_runtime_pr_feedback::PrFeedbackSweepRequestOutcome::ActiveCommandExists {
                workflow_id,
                task_id,
                ..
            } => {
                runtime_submission_response_handle(store, &workflow_id, &TaskId::from_str(&task_id))
                    .await
            }
            crate::workflow_runtime_pr_feedback::PrFeedbackSweepRequestOutcome::NotCandidate {
                workflow_id,
                state,
            } => Err(EnqueueTaskError::BadRequest(format!(
                "workflow runtime rejected PR feedback submission for workflow {workflow_id}: state {state} is not eligible for feedback sweep"
            ))),
            crate::workflow_runtime_pr_feedback::PrFeedbackSweepRequestOutcome::Rejected {
                workflow_id,
                reason,
            } => Err(EnqueueTaskError::BadRequest(format!(
                "workflow runtime rejected PR feedback submission for workflow {workflow_id}: {reason}"
            ))),
        }
    }

    async fn find_runtime_issue_workflow_by_pr(
        &self,
        store: &harness_workflow::runtime::WorkflowRuntimeStore,
        project_id: &str,
        repo: Option<&str>,
        pr_number: u64,
    ) -> Result<Option<harness_workflow::runtime::WorkflowInstance>, EnqueueTaskError> {
        store
            .get_instance_by_pr(GITHUB_ISSUE_PR_DEFINITION_ID, project_id, repo, pr_number)
            .await
            .map_err(|error| EnqueueTaskError::Internal(error.to_string()))
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
        req.project = Some(canonical.clone());

        task_runner::fill_missing_repo_from_project(&mut req).await;
        Self::populate_external_id(&mut req);

        if let Some(definition_id) = req.definition_id.as_deref() {
            crate::workflow_runtime_submission::resolve_declarative_definition_for_project(
                &canonical,
                definition_id,
            )
            .map_err(|error| EnqueueTaskError::BadRequest(error.to_string()))?;
            if self.workflow_runtime_store.is_none() {
                return Err(EnqueueTaskError::Internal(
                    "workflow runtime store is required for declarative workflow submissions"
                        .to_string(),
                ));
            }
            if !self.runtime_prompt_submission_enabled(&canonical)? {
                return Err(EnqueueTaskError::Internal(
                    "workflow runtime dispatch and worker must be enabled for declarative workflow submissions"
                        .to_string(),
                ));
            }
            return Ok(PreparedEnqueueResult::RuntimeDeclarativeSubmission(
                Box::new(PreparedRuntimeDeclarativeSubmission { req, project_id }),
            ));
        }

        if let Some(existing_id) = self.check_workflow_duplicate(&project_id, &req).await {
            return Ok(PreparedEnqueueResult::Existing(existing_id));
        }
        if let Some(existing_id) = self.check_duplicate(&project_id, &req).await {
            return Ok(PreparedEnqueueResult::Existing(existing_id));
        }

        if req.issue.is_some() {
            if self.workflow_runtime_store.is_none() {
                return Err(EnqueueTaskError::Internal(
                    "workflow runtime store is required for GitHub issue submissions".to_string(),
                ));
            }
            if !self.runtime_issue_submission_enabled(&canonical)? {
                return Err(EnqueueTaskError::Internal(
                    "workflow runtime dispatch and worker must be enabled for GitHub issue submissions".to_string(),
                ));
            }
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

        if req.pr.is_some() {
            if self.workflow_runtime_store.is_none() {
                return Err(EnqueueTaskError::Internal(
                    "workflow runtime store is required for PR feedback submissions".to_string(),
                ));
            }
            if !self.runtime_pr_feedback_enabled(&canonical)? {
                return Err(EnqueueTaskError::Internal(
                    "workflow runtime dispatch and worker must be enabled for PR feedback submissions".to_string(),
                ));
            }
            return Ok(PreparedEnqueueResult::RuntimePrFeedbackSubmission(
                Box::new(PreparedRuntimePrFeedbackSubmission { req, project_id }),
            ));
        }

        if req.task_kind() == task_runner::TaskKind::Prompt && req.prompt.is_some() {
            if self.workflow_runtime_store.is_none() {
                return Err(EnqueueTaskError::Internal(
                    "workflow runtime store is required for prompt submissions".to_string(),
                ));
            }
            if !self.runtime_prompt_submission_enabled(&canonical)? {
                return Err(EnqueueTaskError::Internal(
                    "workflow runtime dispatch and worker must be enabled for prompt submissions"
                        .to_string(),
                ));
            }
            if let Some(existing_id) = self
                .check_runtime_prompt_duplicate(&project_id, &req)
                .await?
            {
                return Ok(PreparedEnqueueResult::Existing(existing_id));
            }
            return Ok(PreparedEnqueueResult::RuntimePromptSubmission(Box::new(
                PreparedRuntimePromptSubmission { req, project_id },
            )));
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
        let review_config = resolve_effective_review_config(&self.server_config, &canonical)?;
        let (reviewer, _) = resolve_reviewer(&self.agent_registry, &review_config, agent.name());

        Ok(PreparedEnqueueResult::Ready(Box::new(PreparedEnqueue {
            req,
            project_id,
            agent,
            reviewer,
        })))
    }
}

fn resolve_effective_review_config(
    server_config: &HarnessConfig,
    project_root: &Path,
) -> Result<harness_core::config::agents::AgentReviewConfig, EnqueueTaskError> {
    let project_config =
        harness_core::config::project::load_project_config(project_root).map_err(|error| {
            EnqueueTaskError::Internal(format!(
                "failed to load project config for {}: {error}",
                project_root.display()
            ))
        })?;
    Ok(harness_core::config::resolve::resolve_config(server_config, &project_config).review)
}

fn runtime_prompt_duplicate_allows_resubmission(
    instance: &harness_workflow::runtime::WorkflowInstance,
) -> bool {
    instance.is_terminal() || instance.state == "blocked"
}

async fn runtime_submission_response_handle(
    store: &WorkflowRuntimeStore,
    workflow_id: &str,
    fallback_task_id: &TaskId,
) -> Result<TaskId, EnqueueTaskError> {
    let Some(instance) = store
        .get_instance(workflow_id)
        .await
        .map_err(|error| EnqueueTaskError::Internal(error.to_string()))?
    else {
        return Err(EnqueueTaskError::Internal(format!(
            "workflow runtime accepted submission for missing workflow {workflow_id}"
        )));
    };
    Ok(
        crate::workflow_runtime_submission::runtime_issue_task_handle(&instance)
            .unwrap_or_else(|| fallback_task_id.clone()),
    )
}

fn workflow_runtime_loops_enabled(project_root: &Path) -> Result<bool, EnqueueTaskError> {
    let workflow_cfg =
        harness_core::config::workflow::load_workflow_config(project_root).map_err(|error| {
            EnqueueTaskError::Internal(format!(
                "failed to load workflow runtime config at {}: {error}",
                project_root.display()
            ))
        })?;
    Ok(workflow_cfg.runtime_dispatch.enabled && workflow_cfg.runtime_worker.enabled)
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
            PreparedEnqueueResult::RuntimePromptSubmission(prepared) => {
                return self.submit_prompt_to_workflow_runtime(*prepared).await;
            }
            PreparedEnqueueResult::RuntimeDeclarativeSubmission(prepared) => {
                return self.submit_declarative_to_workflow_runtime(*prepared).await;
            }
            PreparedEnqueueResult::RuntimePrFeedbackSubmission(prepared) => {
                return self.submit_pr_feedback_to_workflow_runtime(*prepared).await;
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
            PreparedEnqueueResult::RuntimePromptSubmission(prepared) => {
                return self.submit_prompt_to_workflow_runtime(*prepared).await;
            }
            PreparedEnqueueResult::RuntimeDeclarativeSubmission(prepared) => {
                return self.submit_declarative_to_workflow_runtime(*prepared).await;
            }
            PreparedEnqueueResult::RuntimePrFeedbackSubmission(prepared) => {
                return self.submit_pr_feedback_to_workflow_runtime(*prepared).await;
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
                    // Terminal-state gate: a task can be cancelled while waiting
                    // for a permit. The cancel HTTP route persists Cancelled and
                    // calls `abort_task`, but for queue-waiting tasks no abort
                    // handle exists yet. Re-read state here and skip execution
                    // if it has already reached a terminal status — dropping
                    // `permit` releases the queue slot without resurrecting the
                    // task to a non-terminal status.
                    if let Some(state) = tasks.get(&task_id2) {
                        if state.status.is_terminal() {
                            tracing::info!(
                                task_id = %task_id2.0,
                                project = %project_id,
                                status = state.status.as_ref(),
                                "skipping execution: task reached terminal status before permit acquisition"
                            );
                            drop(permit);
                            tasks.close_task_stream(&task_id2);
                            return;
                        }
                    }
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
#[path = "execution_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "execution_continuation_tests.rs"]
mod continuation_tests;

#[cfg(test)]
#[path = "execution_declarative_tests.rs"]
mod declarative_tests;
