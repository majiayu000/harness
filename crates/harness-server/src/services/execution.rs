//! Workflow-runtime submission service.
//!
//! All accepted submissions are persisted to the workflow runtime. The former
//! task-runner fallback was unreachable after runtime submission became
//! mandatory for declarative, issue, PR-feedback, and prompt requests.

use crate::{
    project_registry::{check_allowed_roots, ProjectRegistry},
    workflow_runtime_submission::{
        self, CreateTaskRequest, RuntimeDependencyStatus, TaskId, MAX_TASK_PRIORITY,
    },
};
use async_trait::async_trait;
use harness_core::config::HarnessConfig;
use harness_workflow::runtime::{WorkflowRuntimeStore, GITHUB_ISSUE_PR_DEFINITION_ID};
use std::path::{Path, PathBuf};
use std::sync::Arc;

mod runtime_submissions;

pub use crate::workflow_runtime_submission::runtime_models::QueueDomain;

#[derive(Debug)]
pub enum EnqueueTaskError {
    BadRequest(String),
    Internal(String),
    MaintenanceWindow { retry_after_secs: u64 },
}

impl std::fmt::Display for EnqueueTaskError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadRequest(message) => write!(f, "bad request: {message}"),
            Self::Internal(message) => write!(f, "internal error: {message}"),
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

#[async_trait]
pub trait ExecutionService: Send + Sync {
    async fn enqueue(&self, req: CreateTaskRequest) -> Result<TaskId, EnqueueTaskError>;

    async fn enqueue_in_domain(
        &self,
        req: CreateTaskRequest,
        queue_domain: QueueDomain,
    ) -> Result<TaskId, EnqueueTaskError>;

    async fn enqueue_background(&self, req: CreateTaskRequest) -> Result<TaskId, EnqueueTaskError>;
}

pub struct DefaultExecutionService {
    server_config: Arc<HarnessConfig>,
    workflow_runtime_store: Option<Arc<WorkflowRuntimeStore>>,
    project_registry: Option<Arc<ProjectRegistry>>,
    allowed_project_roots: Vec<PathBuf>,
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

enum PreparedEnqueueResult {
    Existing(TaskId),
    RuntimeIssueSubmission(Box<PreparedRuntimeIssueSubmission>),
    RuntimePromptSubmission(Box<PreparedRuntimePromptSubmission>),
    RuntimeDeclarativeSubmission(Box<PreparedRuntimeDeclarativeSubmission>),
    RuntimePrFeedbackSubmission(Box<PreparedRuntimePrFeedbackSubmission>),
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
            Err(error) => Err(EnqueueTaskError::Internal(error.to_string())),
        };
    }

    let id = project_path.to_string_lossy();
    match registry.get(&id).await {
        Ok(Some(project)) => return Ok((Some(project.root), project.default_agent)),
        Ok(None) => {}
        Err(error) => return Err(EnqueueTaskError::Internal(error.to_string())),
    }
    match registry.get_by_name(&id).await {
        Ok(Some(project)) => Ok((Some(project.root), project.default_agent)),
        Ok(None) => Err(EnqueueTaskError::BadRequest(format!(
            "project '{id}' not found in registry and is not a valid directory"
        ))),
        Err(error) => Err(EnqueueTaskError::Internal(error.to_string())),
    }
}

impl DefaultExecutionService {
    pub fn new(
        server_config: Arc<HarnessConfig>,
        workflow_runtime_store: Option<Arc<WorkflowRuntimeStore>>,
        project_registry: Option<Arc<ProjectRegistry>>,
        allowed_project_roots: Vec<PathBuf>,
    ) -> Arc<Self> {
        Arc::new(Self {
            server_config,
            workflow_runtime_store,
            project_registry,
            allowed_project_roots,
        })
    }

    async fn resolve_project(
        &self,
        project: Option<PathBuf>,
    ) -> Result<Option<PathBuf>, EnqueueTaskError> {
        resolve_project_from_registry(self.project_registry.as_deref(), project)
            .await
            .map(|(project, _)| project)
    }

    pub(crate) fn validate_request(req: &CreateTaskRequest) -> Result<(), EnqueueTaskError> {
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
        if req.priority > MAX_TASK_PRIORITY {
            return Err(EnqueueTaskError::BadRequest(format!(
                "priority {} out of range; maximum is {} (0=normal, 1=high, 2=critical)",
                req.priority, MAX_TASK_PRIORITY,
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

    fn check_allowed_roots(&self, canonical: &Path) -> Result<(), EnqueueTaskError> {
        check_allowed_roots(canonical, &self.allowed_project_roots)
            .map_err(EnqueueTaskError::BadRequest)
    }

    fn runtime_submission_enabled(&self, project_root: &Path) -> Result<bool, EnqueueTaskError> {
        workflow_runtime_loops_enabled(project_root)
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
                if id.chars().all(|character| character.is_ascii_digit()) && !id.is_empty() {
                    if req.issue.is_some() {
                        req.external_id = Some(format!("issue:{id}"));
                    } else if req.pr.is_some() {
                        req.external_id = Some(format!("pr:{id}"));
                    }
                }
            }
        }
    }

    async fn dependencies_blocked(
        &self,
        req: &CreateTaskRequest,
    ) -> Result<bool, EnqueueTaskError> {
        for dependency_id in &req.depends_on {
            let status = workflow_runtime_submission::resolve_issue_dependency_status(
                self.workflow_runtime_store.as_deref(),
                dependency_id,
            )
            .await
            .map_err(|error| {
                EnqueueTaskError::Internal(format!(
                    "dependency status lookup failed for {}: {error}",
                    dependency_id.as_str()
                ))
            })?;
            if status != RuntimeDependencyStatus::Done {
                return Ok(true);
            }
        }
        Ok(false)
    }

    async fn serialization_dependencies_blocked(
        &self,
        req: &CreateTaskRequest,
    ) -> Result<bool, EnqueueTaskError> {
        for dependency_id in &req.serialization_depends_on {
            let status = workflow_runtime_submission::resolve_issue_dependency_status(
                self.workflow_runtime_store.as_deref(),
                dependency_id,
            )
            .await
            .map_err(|error| {
                EnqueueTaskError::Internal(format!(
                    "serialization dependency status lookup failed for {}: {error}",
                    dependency_id.as_str()
                ))
            })?;
            if status == RuntimeDependencyStatus::Waiting {
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
        Ok(workflow_runtime_submission::runtime_issue_task_handle(
            &instance,
        ))
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
        let workflow_id = workflow_runtime_submission::prompt_workflow_id(
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
        Ok(workflow_runtime_submission::runtime_issue_task_handle(
            &instance,
        ))
    }

    async fn prepare_enqueue(
        &self,
        mut req: CreateTaskRequest,
    ) -> Result<PreparedEnqueueResult, EnqueueTaskError> {
        let now = chrono::Utc::now();
        if self.server_config.maintenance_window.in_quiet_window(now) {
            return Err(EnqueueTaskError::MaintenanceWindow {
                retry_after_secs: self
                    .server_config
                    .maintenance_window
                    .secs_until_window_end(now),
            });
        }

        Self::validate_request(&req)?;
        req.project = self.resolve_project(req.project).await?;
        let canonical = resolve_canonical_project(req.project.clone())
            .await
            .map_err(|error| EnqueueTaskError::Internal(error.to_string()))?;
        self.check_allowed_roots(&canonical)?;
        let project_id = canonical.to_string_lossy().into_owned();
        req.project = Some(canonical.clone());
        workflow_runtime_submission::fill_missing_repo_from_project(&mut req).await;
        Self::populate_external_id(&mut req);

        if !self.runtime_submission_enabled(&canonical)? {
            return Err(EnqueueTaskError::Internal(
                "workflow runtime dispatch and worker must be enabled for submissions".to_string(),
            ));
        }
        if self.workflow_runtime_store.is_none() {
            return Err(EnqueueTaskError::Internal(
                "workflow runtime store is required for submissions".to_string(),
            ));
        }

        if let Some(definition_id) = req.definition_id.as_deref() {
            workflow_runtime_submission::resolve_declarative_definition_for_project(
                &canonical,
                definition_id,
            )
            .map_err(|error| EnqueueTaskError::BadRequest(error.to_string()))?;
            return Ok(PreparedEnqueueResult::RuntimeDeclarativeSubmission(
                Box::new(PreparedRuntimeDeclarativeSubmission { req, project_id }),
            ));
        }

        if req.issue.is_some() {
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
            return Ok(PreparedEnqueueResult::RuntimePrFeedbackSubmission(
                Box::new(PreparedRuntimePrFeedbackSubmission { req, project_id }),
            ));
        }

        if let Some(existing_id) = self
            .check_runtime_prompt_duplicate(&project_id, &req)
            .await?
        {
            return Ok(PreparedEnqueueResult::Existing(existing_id));
        }
        Ok(PreparedEnqueueResult::RuntimePromptSubmission(Box::new(
            PreparedRuntimePromptSubmission { req, project_id },
        )))
    }

    async fn submit(
        &self,
        req: CreateTaskRequest,
        queue_domain: QueueDomain,
    ) -> Result<TaskId, EnqueueTaskError> {
        match self.prepare_enqueue(req).await? {
            PreparedEnqueueResult::Existing(task_id) => Ok(task_id),
            PreparedEnqueueResult::RuntimeIssueSubmission(prepared) => {
                self.submit_issue_to_workflow_runtime(*prepared).await
            }
            PreparedEnqueueResult::RuntimePromptSubmission(prepared) => {
                self.submit_prompt_to_workflow_runtime(*prepared, queue_domain)
                    .await
            }
            PreparedEnqueueResult::RuntimeDeclarativeSubmission(prepared) => {
                self.submit_declarative_to_workflow_runtime(*prepared).await
            }
            PreparedEnqueueResult::RuntimePrFeedbackSubmission(prepared) => {
                self.submit_pr_feedback_to_workflow_runtime(*prepared).await
            }
        }
    }
}

async fn resolve_canonical_project(project: Option<PathBuf>) -> anyhow::Result<PathBuf> {
    let raw = match project {
        Some(project) => project,
        None => tokio::task::spawn_blocking(
            workflow_runtime_submission::runtime_request::detect_main_worktree,
        )
        .await
        .map_err(|error| anyhow::anyhow!("detect_main_worktree failed: {error}"))?,
    };
    Ok(tokio::fs::canonicalize(&raw).await.unwrap_or(raw))
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
        workflow_runtime_submission::runtime_issue_task_handle(&instance)
            .unwrap_or_else(|| fallback_task_id.clone()),
    )
}

fn workflow_runtime_loops_enabled(project_root: &Path) -> Result<bool, EnqueueTaskError> {
    let workflow_config = harness_core::config::workflow::load_workflow_config(project_root)
        .map_err(|error| {
            EnqueueTaskError::Internal(format!(
                "failed to load workflow runtime config at {}: {error}",
                project_root.display()
            ))
        })?;
    Ok(workflow_config.runtime_dispatch.enabled && workflow_config.runtime_worker.enabled)
}

#[async_trait]
impl ExecutionService for DefaultExecutionService {
    async fn enqueue(&self, req: CreateTaskRequest) -> Result<TaskId, EnqueueTaskError> {
        self.submit(req, QueueDomain::Primary).await
    }

    async fn enqueue_in_domain(
        &self,
        req: CreateTaskRequest,
        queue_domain: QueueDomain,
    ) -> Result<TaskId, EnqueueTaskError> {
        self.submit(req, queue_domain).await
    }

    async fn enqueue_background(&self, req: CreateTaskRequest) -> Result<TaskId, EnqueueTaskError> {
        self.submit(req, QueueDomain::Primary).await
    }
}

#[cfg(test)]
#[path = "execution_continuation_tests.rs"]
mod continuation_tests;

#[cfg(test)]
#[path = "execution_declarative_tests.rs"]
mod declarative_tests;

#[cfg(test)]
#[path = "execution_runtime_policy_tests.rs"]
mod runtime_policy_tests;
