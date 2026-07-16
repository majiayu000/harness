use super::{CreateTaskRequest, DefaultExecutionService, EnqueueTaskError};
use crate::task_runner::TaskId;
use std::path::Path;

pub(super) struct PreparedRuntimeDeclarativeSubmission {
    pub(super) req: CreateTaskRequest,
    pub(super) project_id: String,
}

impl DefaultExecutionService {
    pub(super) fn validate_project_declarative_definition(
        &self,
        project_root: &Path,
        definition_id: &str,
    ) -> Result<(), EnqueueTaskError> {
        crate::workflow_runtime_submission::resolve_project_declarative_definition(
            project_root,
            definition_id,
        )
        .map(|_| ())
        .map_err(|error| EnqueueTaskError::BadRequest(format!("{error:#}")))
    }

    pub(super) async fn check_runtime_declarative_duplicate(
        &self,
        project_id: &str,
        req: &CreateTaskRequest,
    ) -> Result<Option<TaskId>, EnqueueTaskError> {
        let (Some(store), Some(definition_id), Some(external_id)) = (
            self.workflow_runtime_store.as_deref(),
            req.definition_id.as_deref(),
            req.external_id.as_deref(),
        ) else {
            return Ok(None);
        };
        let placeholder = TaskId::from_str("declarative-duplicate-lookup");
        let workflow_id = crate::workflow_runtime_submission::declarative_workflow_id(
            project_id,
            definition_id,
            Some(external_id),
            &placeholder,
        );
        let Some(instance) = store
            .get_instance(&workflow_id)
            .await
            .map_err(|error| EnqueueTaskError::Internal(error.to_string()))?
        else {
            return Ok(None);
        };
        Ok(crate::workflow_runtime_submission::runtime_issue_task_handle(&instance))
    }

    pub(super) async fn submit_declarative_to_workflow_runtime(
        &self,
        prepared: PreparedRuntimeDeclarativeSubmission,
    ) -> Result<TaskId, EnqueueTaskError> {
        let store = self.workflow_runtime_store.as_deref().ok_or_else(|| {
            EnqueueTaskError::Internal(
                "workflow runtime store is required for declarative submissions".to_string(),
            )
        })?;
        let definition_id = prepared.req.definition_id.as_deref().ok_or_else(|| {
            EnqueueTaskError::BadRequest(
                "declarative submission requires definition_id".to_string(),
            )
        })?;
        let prompt = prepared.req.prompt.as_deref().ok_or_else(|| {
            EnqueueTaskError::BadRequest("declarative submission requires a prompt".to_string())
        })?;
        let task_id = TaskId::new();
        let project_root = prepared
            .req
            .project
            .as_deref()
            .unwrap_or_else(|| Path::new(&prepared.project_id));
        let record = crate::workflow_runtime_submission::record_declarative_submission(
            store,
            crate::workflow_runtime_submission::DeclarativeSubmissionRuntimeContext {
                project_root,
                task_id: &task_id,
                definition_id,
                prompt,
                depends_on: &prepared.req.depends_on,
                serialization_depends_on: &prepared.req.serialization_depends_on,
                source: prepared.req.source.as_deref(),
                external_id: prepared.req.external_id.as_deref(),
            },
        )
        .await
        .map_err(|error| EnqueueTaskError::Internal(format!("{error:#}")))?;
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
                "workflow runtime accepted declarative submission for workflow {} without a progress command",
                record.workflow_id
            )));
        }
        tracing::info!(
            workflow_id = %record.workflow_id,
            definition_id,
            task_id = %task_id.0,
            command_count = record.command_ids.len(),
            "execution service: accepted declarative submission into workflow runtime"
        );
        super::runtime_submission_response_handle(store, &record.workflow_id, &task_id).await
    }
}
