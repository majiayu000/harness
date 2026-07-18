use super::*;
use crate::workflow_runtime_submission::{
    prompt_execution_policy,
    runtime_models::{PromptExecutionPolicy, TaskKind},
    runtime_request::SystemTaskInput,
};

#[tokio::test]
async fn review_submission_preserves_runtime_execution_policy() -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let _db_guard = crate::test_helpers::acquire_db_state_guard().await;
    let dir = tempfile::tempdir()?;
    std::fs::write(
        dir.path().join("WORKFLOW.md"),
        "---\nruntime_dispatch:\n  enabled: true\nruntime_worker:\n  enabled: true\n---\n",
    )?;
    let store = Arc::new(
        WorkflowRuntimeStore::open_with_database_url(
            dir.path(),
            Some(&crate::test_helpers::test_database_url()?),
        )
        .await?,
    );
    let service = DefaultExecutionService::new(
        Arc::new(HarnessConfig::default()),
        Some(store.clone()),
        None,
        Vec::new(),
    );
    let prompt = "Review commits since the persisted watermark.".to_string();
    let task_id = service
        .enqueue_in_domain(
            CreateTaskRequest {
                prompt: Some(prompt.clone()),
                agent: Some("claude".to_string()),
                project: Some(dir.path().to_path_buf()),
                turn_timeout_secs: 91,
                source: Some("periodic_review".to_string()),
                external_id: Some("periodic-review:policy-test".to_string()),
                priority: 2,
                system_input: Some(SystemTaskInput::PeriodicReview { prompt }),
                ..CreateTaskRequest::default()
            },
            QueueDomain::Review,
        )
        .await?;

    let workflow = store
        .get_instance_by_submission_id(task_id.as_str())
        .await?
        .expect("review workflow should be persisted");
    assert_eq!(
        prompt_execution_policy(&workflow.data)?,
        Some(PromptExecutionPolicy {
            task_kind: TaskKind::Review,
            agent: Some("claude".to_string()),
            turn_timeout_secs: Some(91),
            queue_domain: QueueDomain::Review,
            priority: 2,
        })
    );
    Ok(())
}
