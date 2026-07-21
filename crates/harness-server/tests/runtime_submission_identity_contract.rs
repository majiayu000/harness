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
        "request_id",
        "POST /api/workflows/runtime/submissions",
        "GET /api/workflows/runtime/submissions",
        "GET /api/workflows/runtime/submissions/{id}",
        "GET /api/workflows/runtime/submissions/{id}/stream",
        "GET /api/workflows/runtime/submissions/{id}/proof",
        "GET /api/workflows/runtime/submissions/{id}/artifacts",
        "GET /api/workflows/runtime/submissions/{id}/prompts",
        "POST /api/workflows/runtime/cancel",
        "POST /api/workflows/runtime/merge",
        "POST /api/workflows/runtime/unblock",
        "POST /api/workflows/runtime/retry",
        "POST /api/workflows/runtime/turns/{turn_id}/approvals/{request_id}",
        "Removed Compatibility",
    ] {
        assert!(
            doc.contains(required),
            "identity contract should document {required}"
        );
    }

    let (current_contract, removed_compatibility) = doc
        .split_once("## Removed Compatibility")
        .expect("identity contract should separate removed compatibility routes");
    assert!(
        !current_contract.contains("/tasks"),
        "current identity contract must not advertise removed /tasks routes"
    );
    assert!(
        removed_compatibility.contains("have been removed"),
        "removed compatibility section should explicitly mark /tasks routes as removed"
    );
}

#[tokio::test]
async fn runtime_store_resolves_legacy_submission_aliases() -> anyhow::Result<()> {
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
        .get_instance_by_submission_id("runtime-handle-second")
        .await?
        .expect("current submission handle should resolve");
    let historical = store
        .get_instance_by_submission_id("runtime-handle-first")
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

#[tokio::test]
async fn runtime_store_uses_explicit_submission_id_as_public_handle() -> anyhow::Result<()> {
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
        WorkflowSubject::new("issue", "issue:1129"),
    )
    .with_id("explicit-submission-workflow")
    .with_data(json!({
        "submission_id": "stable-submission",
        "task_id": "retry-task",
        "task_ids": ["stable-submission", "retry-task"],
        "project_id": "/project-a",
        "repo": "owner/repo",
        "issue_number": 1129
    }));
    store.upsert_instance(&workflow).await?;

    let current = store
        .get_instance_by_submission_id("stable-submission")
        .await?
        .expect("explicit submission id should resolve");
    let retry_alias = store.get_instance_by_submission_id("retry-task").await?;

    assert_eq!(current.id, workflow.id);
    assert!(
        retry_alias.is_none(),
        "task_id must not remain a public lookup alias once submission_id is explicit"
    );
    Ok(())
}
