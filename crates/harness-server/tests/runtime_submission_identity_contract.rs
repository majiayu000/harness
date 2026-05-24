use harness_workflow::runtime::{
    WorkflowInstance, WorkflowRuntimeStore, WorkflowSubject, GITHUB_ISSUE_PR_DEFINITION_ID,
};
use serde_json::json;

#[test]
fn identity_contract_doc_covers_runtime_submission_api_surface() {
    let doc = include_str!("../../../docs/runtime-submission-identity-contract.md");
    for required in [
        "workflow_id",
        "submission_id",
        "task_id",
        "POST /tasks",
        "GET /tasks",
        "GET /tasks/{id}",
        "GET /tasks/{id}/stream",
        "POST /tasks/{id}/cancel",
        "GET /tasks/{id}/proof",
        "GET /tasks/{id}/artifacts",
        "GET /tasks/{id}/prompts",
        "Phase 0: Current Compatibility",
        "Phase 1: Add Explicit `submission_id`",
        "Phase 2: Prefer Runtime-Native Handles",
        "Phase 3: Remove Legacy Compatibility",
    ] {
        assert!(
            doc.contains(required),
            "identity contract should document {required}"
        );
    }
}

#[tokio::test]
async fn runtime_store_resolves_current_and_historical_submission_aliases() -> anyhow::Result<()> {
    let Some(database_url) = harness_core::db::resolve_database_url(None).ok() else {
        return Ok(());
    };

    let dir = tempfile::tempdir()?;
    let store =
        WorkflowRuntimeStore::open_with_database_url(dir.path(), Some(&database_url)).await?;
    let workflow = WorkflowInstance::new(
        GITHUB_ISSUE_PR_DEFINITION_ID,
        1,
        "implementing",
        WorkflowSubject::new("issue", "issue:1127"),
    )
    .with_id("identity-contract-workflow")
    .with_data(json!({
        "task_id": "runtime-handle-second",
        "task_ids": ["runtime-handle-first", "runtime-handle-second"],
        "project_id": "/project-a",
        "repo": "owner/repo",
        "issue_number": 1127
    }));
    store.upsert_instance(&workflow).await?;

    let current = store
        .get_instance_by_task_id("runtime-handle-second")
        .await?
        .expect("current submission handle should resolve");
    let historical = store
        .get_instance_by_task_id("runtime-handle-first")
        .await?
        .expect("historical submission alias should resolve");

    assert_eq!(current.id, workflow.id);
    assert_eq!(historical.id, workflow.id);
    assert_eq!(current.data["task_id"], "runtime-handle-second");
    assert_eq!(
        current.data["task_ids"],
        json!(["runtime-handle-first", "runtime-handle-second"])
    );
    Ok(())
}
