use super::*;
use harness_agents::registry::AgentRegistry;
use harness_core::config::HarnessConfig;
use harness_workflow::runtime::{
    build_declarative_definition, register_declarative_workflow_definitions, WorkflowRuntimeStore,
};
use std::sync::Arc;

const SERVICE_DEFINITION_ID: &str = "declarative_execution_service_v1";

fn write_service_workflow(project_root: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(project_root)?;
    std::fs::write(
        project_root.join("WORKFLOW.md"),
        format!(
            r#"---
definition:
  id: {SERVICE_DEFINITION_ID}
  initial: reviewing
  states:
    reviewing:
      activity: review_docs
      on_success: done
      on_failure: failed
      on_blocked: blocked
      on_signal:
        cancel: cancelled
    blocked:
      progress: operator_gate
  terminal:
    done: succeeded
    failed: failed
    cancelled: cancelled
  recovery_targets: [reviewing]
activities:
  review_docs:
    prompt: Review the submitted documentation.
---
Run the declared documentation review.
"#,
        ),
    )?;
    Ok(())
}

async fn service_with_runtime_store(
    task_store: Arc<TaskStore>,
    runtime_store: Arc<WorkflowRuntimeStore>,
) -> anyhow::Result<Arc<DefaultExecutionService>> {
    let event_path = tempfile::tempdir()?.keep();
    let events = Arc::new(harness_observe::event_store::EventStore::new(&event_path).await?);
    let queue = || {
        Arc::new(TaskQueue::new(
            &harness_core::config::misc::ConcurrencyConfig::default(),
        ))
    };
    Ok(DefaultExecutionService::new(
        task_store,
        Arc::new(AgentRegistry::new("test")),
        Arc::new(HarnessConfig::default()),
        Default::default(),
        events,
        Vec::new(),
        None,
        queue(),
        queue(),
        None,
        None,
        Some(runtime_store),
        None,
        Vec::new(),
    ))
}

#[test]
fn declarative_request_requires_prompt_only_task() {
    let request = CreateTaskRequest {
        definition_id: Some("review_docs_v1".to_string()),
        issue: Some(42),
        ..Default::default()
    };

    let error = DefaultExecutionService::validate_request(&request)
        .expect_err("issue submissions must reject definition_id");
    assert!(error
        .to_string()
        .contains("definition_id is only supported for prompt tasks"));
}

#[test]
fn declarative_request_rejects_dependencies() {
    let request = CreateTaskRequest {
        prompt: Some("Review the release notes.".to_string()),
        definition_id: Some("review_docs_v1".to_string()),
        depends_on: vec![TaskId::from_str("upstream-task")],
        ..Default::default()
    };

    let error = DefaultExecutionService::validate_request(&request)
        .expect_err("v1 declarative submissions must reject dependencies");
    assert!(error
        .to_string()
        .contains("does not support depends_on in v1"));
}

#[test]
fn declarative_request_accepts_prompt_and_external_id() {
    let request = CreateTaskRequest {
        prompt: Some("Review the release notes.".to_string()),
        definition_id: Some("review_docs_v1".to_string()),
        external_id: Some("release-2026-07".to_string()),
        ..Default::default()
    };

    DefaultExecutionService::validate_request(&request)
        .expect("valid declarative prompt request should pass validation");
}

#[tokio::test]
async fn declarative_request_enters_runtime_once_for_stable_external_id() -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }
    let sandbox = tempfile::tempdir()?;
    let project_root = sandbox.path().join("project");
    write_service_workflow(&project_root)?;
    let document = harness_core::config::workflow::load_workflow_document(&project_root)?;
    let policy = document
        .config
        .definition
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("fixture definition is missing"))?;
    let definition = build_declarative_definition(policy, &document.config.activities)?;
    register_declarative_workflow_definitions([definition])?;

    let database_url = crate::test_helpers::test_database_url()?;
    let task_store = TaskStore::open(&sandbox.path().join("tasks")).await?;
    let runtime_store = Arc::new(
        WorkflowRuntimeStore::open_with_database_url(
            &sandbox.path().join("runtime"),
            Some(&database_url),
        )
        .await?,
    );
    let service = service_with_runtime_store(task_store.clone(), runtime_store.clone()).await?;
    let request = CreateTaskRequest {
        prompt: Some("Review the release documentation.".to_string()),
        definition_id: Some(SERVICE_DEFINITION_ID.to_string()),
        project: Some(project_root.clone()),
        external_id: Some("release-2026-07".to_string()),
        ..Default::default()
    };

    let first_handle = service.enqueue_background(request.clone()).await?;
    let duplicate_handle = service.enqueue_background(request).await?;

    assert_eq!(duplicate_handle, first_handle);
    assert!(task_store
        .get_with_db_fallback(&first_handle)
        .await?
        .is_none());
    let project_id = project_root.canonicalize()?.to_string_lossy().into_owned();
    let workflow_id = crate::workflow_runtime_submission::declarative_workflow_id(
        &project_id,
        SERVICE_DEFINITION_ID,
        Some("release-2026-07"),
        &first_handle,
    );
    let workflow = runtime_store
        .get_instance(&workflow_id)
        .await?
        .expect("declarative workflow should be submitted");
    assert_eq!(workflow.data["submission_id"], first_handle.as_str());
    assert_eq!(runtime_store.commands_for(&workflow_id).await?.len(), 1);
    Ok(())
}
