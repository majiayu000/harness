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
    let expires_at = Utc::now() + Duration::hours(1);

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
            .expire_workflow_run_evidence_payloads(expires_at + Duration::minutes(1), 100)
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
              AND table_schema = current_schema()
        )",
    )
    .fetch_one(store.pool())
    .await?;
    assert!(table_exists);

    let (index_count,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*)
         FROM pg_indexes
         WHERE tablename = 'workflow_run_evidence'
           AND schemaname = current_schema()
           AND indexname = ANY($1::text[])",
    )
    .bind(vec![
        "idx_workflow_run_evidence_project_time",
        "idx_workflow_run_evidence_commit_time",
        "idx_workflow_run_evidence_suite_time",
        "idx_workflow_run_evidence_decision_time",
        "idx_workflow_run_evidence_created_time",
        "idx_workflow_run_evidence_payload_expiry",
        "idx_workflow_run_evidence_workflow_id",
        "idx_workflow_run_evidence_command_id",
        "idx_workflow_run_evidence_runtime_job_id",
    ])
    .fetch_one(store.pool())
    .await?;
    assert_eq!(index_count, 9);
    Ok(())
}

#[tokio::test]
async fn runtime_completion_persists_run_evidence_and_pruning_keeps_metadata() -> anyhow::Result<()>
{
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let workflow = project_issue_instance("/project-completion-evidence", 1757, "implementing")
        .with_id("completion-evidence-workflow")
        .with_server_data(json!({
            "project_id": "/project-completion-evidence",
            "issue_number": 1757,
            "base_ref": "origin/main",
            "pr_head_sha": "workflow-head"
        }));
    store
        .force_upsert_lifecycle_state_for_test(&workflow)
        .await?;
    let job = enqueue_workflow_runtime_job(
        &store,
        &workflow.id,
        "completion-evidence",
        RuntimeKind::CodexJsonrpc,
        "codex-default",
        json!({
            "activity": "implement_issue",
            "command": {"base_ref": "origin/main"}
        }),
        None,
    )
    .await?;
    let lease_expires_at = Utc::now() + Duration::minutes(5);
    let claimed = store
        .claim_next_runtime_job("completion-evidence-worker", lease_expires_at)
        .await?
        .expect("runtime job should be claimable");
    assert_eq!(claimed.id, job.id);
    store
        .record_runtime_event(
            &claimed.id,
            "RuntimePromptPrepared",
            json!({
                "prompt_packet_digest": "sha256:prompt",
                "prompt_packet": {
                    "project": {"root": "/project-completion-evidence"},
                    "workflow_file": {
                        "config": {
                            "base": {"remote": "origin", "branch": "main"}
                        }
                    }
                }
            }),
        )
        .await?;
    let result = ActivityResult::failed("implement_issue", "runtime failed", "agent failed");
    let completion = store
        .commit_runtime_activity_completion_if_owned(
            &claimed.id,
            "completion-evidence-worker",
            lease_expires_at,
            &result,
        )
        .await?
        .expect("completion should commit");
    let completion_event = completion
        .workflow_event
        .as_ref()
        .expect("workflow completion event should be recorded");

    let records = store
        .query_workflow_run_evidence(crate::runtime::WorkflowRunEvidenceQuery {
            project_id: Some("/project-completion-evidence".to_string()),
            suite: Some("implement_issue".to_string()),
            include_payload: true,
            limit: 10,
            ..Default::default()
        })
        .await?;
    assert_eq!(records.len(), 1);
    let record = &records[0];
    assert_eq!(record.workflow_id, workflow.id);
    assert_eq!(record.command_id.as_deref(), Some(job.command_id.as_str()));
    assert_eq!(record.runtime_job_id.as_deref(), Some(job.id.as_str()));
    assert_eq!(record.commit_sha.as_deref(), Some("workflow-head"));
    assert_eq!(record.stack, "codex-default");
    assert_eq!(record.baseline.as_deref(), Some("origin/main"));
    assert_eq!(
        record.evidence_schema,
        crate::runtime::WORKFLOW_RUN_EVIDENCE_SCHEMA
    );
    assert!(record.digest.starts_with("sha256:"));
    assert_eq!(record.trust, "server_persisted_runtime_completion");
    assert_eq!(
        record.location["workflow_event_id"],
        json!(completion_event.id.clone())
    );
    assert_eq!(record.location["prompt_packet_digest"], "sha256:prompt");
    assert_eq!(
        record.payload.as_ref().and_then(|payload| payload
            .get("activity_result")
            .and_then(|result| result.get("status"))),
        Some(&json!("failed"))
    );

    let mut terminal = store
        .get_instance(&workflow.id)
        .await?
        .expect("workflow should still exist");
    terminal.state = "done".to_string();
    store
        .force_upsert_lifecycle_state_for_test(&terminal)
        .await?;
    let summary = store
        .prune_terminal_runtime_history(Utc::now() + Duration::days(1), 100)
        .await?;
    assert_eq!(summary.workflow_instances_deleted, 1);

    let preserved = store
        .query_workflow_run_evidence(crate::runtime::WorkflowRunEvidenceQuery {
            project_id: Some("/project-completion-evidence".to_string()),
            include_payload: false,
            limit: 10,
            ..Default::default()
        })
        .await?;
    assert_eq!(preserved.len(), 1);
    assert_eq!(preserved[0].workflow_id, workflow.id);
    assert_eq!(
        preserved[0].command_id.as_deref(),
        Some(job.command_id.as_str())
    );
    assert_eq!(
        preserved[0].runtime_job_id.as_deref(),
        Some(job.id.as_str())
    );
    assert_eq!(preserved[0].payload, None);
    Ok(())
}
