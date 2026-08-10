use super::*;

#[tokio::test]
async fn workflow_run_evidence_persists_queries_exports_and_expires_payloads() -> anyhow::Result<()>
{
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let workflow = project_issue_instance("/project-evidence", 1757, "implementing")
        .with_id("evidence-workflow");
    store
        .force_upsert_lifecycle_state_for_test(&workflow)
        .await?;
    let job = enqueue_workflow_runtime_job(
        &store,
        &workflow.id,
        "evidence",
        RuntimeKind::CodexJsonrpc,
        "codex-default",
        json!({"activity": "implement_issue"}),
        None,
    )
    .await?;
    let expires_at = Utc::now() - Duration::minutes(5);

    let record = store
        .record_workflow_run_evidence(crate::runtime::WorkflowRunEvidenceInput {
            id: Some("evidence-1".to_string()),
            workflow_id: workflow.id.clone(),
            command_id: None,
            runtime_job_id: Some(job.id.clone()),
            project_id: "/project-evidence".to_string(),
            commit_sha: Some("abc123".to_string()),
            stack: "codex-default".to_string(),
            suite: "acceptance".to_string(),
            baseline: Some("origin/main".to_string()),
            decision: "accepted".to_string(),
            evidence_schema: "harness.test.evidence.v1".to_string(),
            digest: "sha256:abc123".to_string(),
            trust: "agent_reported_sanitized".to_string(),
            location: json!({"kind": "workflow_artifact", "artifact_ref": "artifact-1"}),
            retention_class: "short".to_string(),
            payload: Some(json!({"bounded": true})),
            payload_expires_at: Some(expires_at),
        })
        .await?;

    assert_eq!(record.runtime_job_id.as_deref(), Some(job.id.as_str()));
    assert_eq!(record.command_id.as_deref(), Some(job.command_id.as_str()));

    let records = store
        .query_workflow_run_evidence(crate::runtime::WorkflowRunEvidenceQuery {
            project_id: Some("/project-evidence".to_string()),
            commit_sha: Some("abc123".to_string()),
            suite: Some("acceptance".to_string()),
            decision: Some("accepted".to_string()),
            created_after: Some(Utc::now() - Duration::hours(1)),
            include_payload: true,
            limit: 10,
            ..Default::default()
        })
        .await?;
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].payload, Some(json!({"bounded": true})));

    let metadata_only = store
        .query_workflow_run_evidence(crate::runtime::WorkflowRunEvidenceQuery {
            project_id: Some("/project-evidence".to_string()),
            include_payload: false,
            limit: 10,
            ..Default::default()
        })
        .await?;
    assert_eq!(metadata_only[0].payload, None);
    assert_eq!(metadata_only[0].digest, "sha256:abc123");
    assert_eq!(metadata_only[0].location["artifact_ref"], "artifact-1");

    let export = store
        .export_workflow_run_evidence(crate::runtime::WorkflowRunEvidenceQuery {
            project_id: Some("/project-evidence".to_string()),
            include_payload: true,
            limit: 10,
            ..Default::default()
        })
        .await?;
    assert_eq!(
        export.schema,
        crate::runtime::WORKFLOW_RUN_EVIDENCE_EXPORT_SCHEMA
    );
    assert_eq!(export.limit, 10);
    assert_eq!(export.records.len(), 1);

    assert_eq!(
        store
            .expire_workflow_run_evidence_payloads(Utc::now(), 100)
            .await?,
        1
    );
    let expired = store
        .query_workflow_run_evidence(crate::runtime::WorkflowRunEvidenceQuery {
            project_id: Some("/project-evidence".to_string()),
            include_payload: true,
            limit: 10,
            ..Default::default()
        })
        .await?;
    assert_eq!(expired[0].payload, None);
    assert!(expired[0].payload_expired_at.is_some());
    assert_eq!(expired[0].digest, "sha256:abc123");
    assert_eq!(expired[0].trust, "agent_reported_sanitized");
    assert_eq!(expired[0].retention_class, "short");
    Ok(())
}

#[tokio::test]
async fn workflow_run_evidence_migration_creates_query_indexes() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let (table_exists,): (bool,) = sqlx::query_as(
        "SELECT EXISTS (
            SELECT 1
            FROM information_schema.tables
            WHERE table_name = 'workflow_run_evidence'
        )",
    )
    .fetch_one(store.pool())
    .await?;
    assert!(table_exists);

    let (index_count,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*)
         FROM pg_indexes
         WHERE tablename = 'workflow_run_evidence'
           AND indexname = ANY($1::text[])",
    )
    .bind(vec![
        "idx_workflow_run_evidence_project_time",
        "idx_workflow_run_evidence_commit_time",
        "idx_workflow_run_evidence_suite_time",
        "idx_workflow_run_evidence_decision_time",
        "idx_workflow_run_evidence_payload_expiry",
    ])
    .fetch_one(store.pool())
    .await?;
    assert_eq!(index_count, 5);
    Ok(())
}
