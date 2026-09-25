#[tokio::test]
async fn rejected_initial_failure_serializes_against_command_and_job_enqueue(
) -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let initial = issue_instance("discovered").with_id("submission-rejected-enqueue-race");
    let decision = WorkflowDecision::new(
        &initial.id,
        "discovered",
        "plan_issue",
        "planning",
        "submission would have started planning",
    );
    let mut failed = initial.clone();
    failed.state = "failed".to_string();
    failed.version = 1;
    let command = WorkflowCommand::enqueue_activity(
        "plan_issue",
        "submission-rejected-enqueue-race-command",
    );

    let submission = store.commit_submission_decision_transition(
        crate::runtime::WorkflowSubmissionDecisionTransition {
            workflow_id: &initial.id,
            expected_state: &initial.state,
            expected_version: initial.version,
            create_if_missing: Some(&initial),
            event_id: None,
            new_event_id: Some("submission-rejected-enqueue-race-event"),
            event_type: "IssueSubmitted",
            source: "workflow-runtime-test",
            payload: json!({}),
            decision: &decision,
            existing_record: None,
            rejection_reason: Some("submission rejected"),
            final_instance: Some(&failed),
            command_status: WorkflowCommandStatus::Pending,
            prompt_payload: None,
        },
    );
    let enqueue = async {
        let command_id = store.enqueue_command(&initial.id, None, &command).await?;
        let job = store
            .enqueue_runtime_job(
                &command_id,
                RuntimeKind::CodexJsonrpc,
                "codex-default",
                json!({"activity": "plan_issue"}),
            )
            .await?;
        Ok::<_, anyhow::Error>((command_id, job.id))
    };
    let (submission, enqueue) = tokio::join!(submission, enqueue);

    let commit = submission?.expect("rejected submission should commit its failed instance");
    assert!(!commit.record.accepted);
    assert!(enqueue.is_err(), "terminal admission must reject racing work");
    assert_eq!(
        store
            .get_instance(&initial.id)
            .await?
            .expect("failed submission should remain auditable")
            .state,
        "failed"
    );
    let (active_commands,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM workflow_commands
         WHERE workflow_id = $1
           AND status IN ('pending', 'dispatching', 'dispatched', 'deferred')",
    )
    .bind(&initial.id)
    .fetch_one(store.pool())
    .await?;
    let (active_jobs,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM runtime_jobs AS job
         JOIN workflow_commands AS command ON command.id = job.command_id
         WHERE command.workflow_id = $1 AND job.status IN ('pending', 'running')",
    )
    .bind(&initial.id)
    .fetch_one(store.pool())
    .await?;
    assert_eq!(active_commands, 0);
    assert_eq!(active_jobs, 0);
    Ok(())
}
