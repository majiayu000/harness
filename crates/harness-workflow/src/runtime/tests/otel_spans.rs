use super::*;

#[tokio::test]
async fn otel_spans_persist_trace_context_on_workflow_row() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&harness_core::config::dirs::default_db_path(
        dir.path(),
        "workflow_runtime",
    ))
    .await?;
    let workflow = WorkflowInstance::new(
        GITHUB_ISSUE_PR_DEFINITION_ID,
        1,
        "implementing",
        WorkflowSubject::new("issue", "issue:1451"),
    )
    .with_id("otel-trace-workflow");
    store.upsert_instance(&workflow).await?;

    let first = store
        .ensure_otel_trace_context(&workflow.id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("trace context should be created"))?;
    let second = store
        .ensure_otel_trace_context(&workflow.id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("trace context should be reused"))?;
    let loaded = store
        .get_instance(&workflow.id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("workflow should exist"))?;

    assert_eq!(first, second);
    assert!(first.has_valid_trace_ids());
    assert_eq!(
        loaded.data["otel_trace_context"]["trace_id"].as_str(),
        Some(first.trace_id.as_str())
    );
    assert_eq!(
        loaded.data["otel_trace_context"]["root_span_id"].as_str(),
        Some(first.root_span_id.as_str())
    );
    Ok(())
}
