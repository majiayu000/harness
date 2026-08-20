#[tokio::test]
async fn terminal_completion_serializes_against_command_enqueue() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let instance = issue_instance("implementing").with_id("terminal-enqueue-race");
    store
        .force_upsert_lifecycle_state_for_test(&instance)
        .await?;

    let late_command = WorkflowCommand::enqueue_activity(
        "inspect_pr_feedback",
        "terminal-enqueue-race-command",
    );
    let result = ActivityResult::succeeded(
        "implement_issue",
        "The issue closed while another command was being enqueued.",
    )
    .with_signal(ActivitySignal::new(
        "IssueClosed",
        json!({
            "issue_number": 123,
            "state": "closed",
            "issue_url": "https://github.com/owner/repo/issues/123"
        }),
    ))
    .with_artifact(crate::runtime::completion_evidence::verified_issue_state_for_test(123));

    let enqueue = store.enqueue_command(&instance.id, None, &late_command);
    let completion = store.commit_parent_runtime_completion(
        &instance.id,
        "runtime-terminal-race",
        json!({
            "command_id": "terminal-enqueue-race-completed-command",
            "runtime_job_id": "terminal-enqueue-race-completed-job",
            "activity_result": result,
        }),
    );
    let (enqueue, completion) = tokio::join!(enqueue, completion);
    let record = completion?.expect("closed issue should produce a terminal decision");
    assert!(record.accepted);
    assert_eq!(record.decision.next_state, "done");

    match enqueue {
        Ok(command_id) => {
            assert_eq!(
                store
                    .get_command(&command_id)
                    .await?
                    .expect("racing command should remain auditable")
                    .status,
                WorkflowCommandStatus::Cancelled
            );
        }
        Err(error) => assert!(
            error.to_string().contains("terminal workflow"),
            "unexpected enqueue error: {error}"
        ),
    }
    let active_statuses = [
        WorkflowCommandStatus::Pending,
        WorkflowCommandStatus::Dispatching,
        WorkflowCommandStatus::Dispatched,
        WorkflowCommandStatus::Deferred,
    ];
    assert!(store
        .commands_for(&instance.id)
        .await?
        .iter()
        .all(|command| !active_statuses.contains(&command.status)));
    Ok(())
}

#[tokio::test]
async fn terminal_completion_serializes_against_raw_runtime_job_enqueue() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let instance = issue_instance("implementing").with_id("terminal-runtime-job-race");
    store
        .force_upsert_lifecycle_state_for_test(&instance)
        .await?;
    let command =
        WorkflowCommand::enqueue_activity("inspect_pr_feedback", "terminal-runtime-job-command");
    let command_id = store.enqueue_command(&instance.id, None, &command).await?;
    let result = ActivityResult::succeeded(
        "implement_issue",
        "The issue closed while a runtime job was being enqueued.",
    )
    .with_signal(ActivitySignal::new(
        "IssueClosed",
        json!({
            "issue_number": 123,
            "state": "closed",
            "issue_url": "https://github.com/owner/repo/issues/123"
        }),
    ))
    .with_artifact(crate::runtime::completion_evidence::verified_issue_state_for_test(123));

    let enqueue = store.enqueue_runtime_job(
        &command_id,
        RuntimeKind::CodexJsonrpc,
        "codex-default",
        json!({"activity": "inspect_pr_feedback"}),
    );
    let completion = store.commit_parent_runtime_completion(
        &instance.id,
        "runtime-terminal-job-race",
        json!({
            "command_id": "terminal-runtime-job-completed-command",
            "runtime_job_id": "terminal-runtime-job-completed-job",
            "activity_result": result,
        }),
    );
    let (enqueue, completion) = tokio::join!(enqueue, completion);
    let record = completion?.expect("closed issue should produce a terminal decision");
    assert!(record.accepted);
    assert_eq!(record.decision.next_state, "done");

    if let Ok(job) = enqueue {
        assert_eq!(
            store
                .get_runtime_job(&job.id)
                .await?
                .expect("racing runtime job should remain auditable")
                .status,
            RuntimeJobStatus::Cancelled
        );
    }
    let (unfinished_jobs,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM runtime_jobs
         WHERE command_id = $1 AND status IN ('pending', 'running')",
    )
    .bind(&command_id)
    .fetch_one(store.pool())
    .await?;
    assert_eq!(unfinished_jobs, 0);
    assert_eq!(
        store
            .get_command(&command_id)
            .await?
            .expect("terminal command should remain auditable")
            .status,
        WorkflowCommandStatus::Cancelled
    );
    Ok(())
}

#[tokio::test]
async fn legacy_unfinished_jobs_cannot_cross_a_persisted_terminal_fence() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let mut instance = issue_instance("implementing").with_id("legacy-terminal-runtime-jobs");
    store
        .force_upsert_lifecycle_state_for_test(&instance)
        .await?;

    let running_command =
        WorkflowCommand::enqueue_activity("implement_issue", "legacy-terminal-running-command");
    let running_command_id = store
        .enqueue_command(&instance.id, None, &running_command)
        .await?;
    let running_job = store
        .enqueue_runtime_job(
            &running_command_id,
            RuntimeKind::RemoteHost,
            "remote-host",
            json!({"activity": "implement_issue"}),
        )
        .await?;
    let lease_expires_at = Utc::now() + Duration::minutes(5);
    let claimed = store
        .claim_next_runtime_job_for_runtime_kind(
            RuntimeKind::RemoteHost,
            "legacy-terminal-host",
            lease_expires_at,
        )
        .await?
        .expect("legacy running job should be claimed before terminalization");
    assert_eq!(claimed.id, running_job.id);

    let pending_command =
        WorkflowCommand::enqueue_activity("inspect_pr_feedback", "legacy-terminal-pending-command");
    let pending_command_id = store
        .enqueue_command(&instance.id, None, &pending_command)
        .await?;
    let pending_job = store
        .enqueue_runtime_job(
            &pending_command_id,
            RuntimeKind::RemoteHost,
            "remote-host",
            json!({"activity": "inspect_pr_feedback"}),
        )
        .await?;
    store
        .mark_command_status(&pending_command_id, WorkflowCommandStatus::Failed)
        .await?;

    // Reproduce a database written before terminal transitions fenced their
    // children: the test-only upsert intentionally bypasses transition hooks.
    instance.state = "done".to_string();
    store
        .force_upsert_lifecycle_state_for_test(&instance)
        .await?;

    let result = ActivityResult::succeeded("implement_issue", "stale terminal completion");
    assert!(store
        .complete_runtime_job_if_owned(
            &running_job.id,
            "legacy-terminal-host",
            lease_expires_at,
            &result,
        )
        .await?
        .is_none());
    assert!(store
        .commit_runtime_activity_completion_if_owned(
            &running_job.id,
            "legacy-terminal-host",
            lease_expires_at,
            &result,
        )
        .await?
        .is_none());

    assert!(store
        .claim_next_runtime_job_for_runtime_kind(
            RuntimeKind::RemoteHost,
            "late-terminal-host",
            Utc::now() + Duration::minutes(5),
        )
        .await?
        .is_none());
    for job_id in [&running_job.id, &pending_job.id] {
        assert_eq!(
            store
                .get_runtime_job(job_id)
                .await?
                .expect("legacy terminal job should remain auditable")
                .status,
            RuntimeJobStatus::Cancelled
        );
    }
    assert_eq!(
        store
            .get_command(&running_command_id)
            .await?
            .expect("active terminal command should remain auditable")
            .status,
        WorkflowCommandStatus::Cancelled
    );
    assert_eq!(
        store
            .get_command(&pending_command_id)
            .await?
            .expect("legacy failed command should remain auditable")
            .status,
        WorkflowCommandStatus::Failed
    );
    Ok(())
}

#[tokio::test]
async fn legacy_terminal_running_eval_cannot_renew_an_unexpired_lease() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let mut instance =
        issue_instance("implementing").with_id("legacy-terminal-running-eval-renewal");
    store
        .force_upsert_lifecycle_state_for_test(&instance)
        .await?;
    let command = WorkflowCommand::enqueue_activity(
        "implement_issue",
        "legacy-terminal-running-eval-command",
    );
    let command_id = store.enqueue_command(&instance.id, None, &command).await?;
    store
        .enqueue_runtime_job(
            &command_id,
            RuntimeKind::RemoteHost,
            "remote-host",
            json!({
                "activity": "implement_issue",
                "eval": {"timeout_secs": 60}
            }),
        )
        .await?;
    let lease_expires_at = Utc::now() + Duration::minutes(5);
    let claimed = store
        .claim_next_runtime_job_for_runtime_kind(
            RuntimeKind::RemoteHost,
            "legacy-terminal-renewal-host",
            lease_expires_at,
        )
        .await?
        .expect("legacy running eval should be claimed before terminalization");
    let lease_proof = store
        .remote_runtime_job_lease_proof(
            &claimed.id,
            "legacy-terminal-renewal-host",
            claimed.lease_generation,
            lease_expires_at,
        )
        .await?;

    // Reproduce a terminal row written before terminal fencing existed. There
    // is deliberately no second pending job to make the claim path repair it.
    instance.state = "done".to_string();
    store
        .force_upsert_lifecycle_state_for_test(&instance)
        .await?;

    let outcome = store
        .renew_remote_host_runtime_job_lease(
            crate::runtime::store::runtime_job_leases::RuntimeJobLeaseRenewalRequest {
                runtime_job_id: &claimed.id,
                owner: "legacy-terminal-renewal-host",
                lease_generation: claimed.lease_generation,
                lease_proof,
                previous_expires_at: lease_expires_at,
                renewal_id: uuid::Uuid::new_v4(),
                lease_secs: 60,
                now: Utc::now(),
                max_lease_secs: 3_600,
                owner_active: true,
            },
        )
        .await?;
    assert_eq!(
        outcome,
        crate::runtime::store::runtime_job_leases::RuntimeJobLeaseRenewalOutcome::LeaseLost {
            reason: crate::runtime::store::runtime_job_leases::RuntimeJobLeaseRenewalRejection::CancellationRequested,
        }
    );
    let fenced = store
        .get_runtime_job(&claimed.id)
        .await?
        .expect("legacy terminal eval should remain auditable");
    assert_eq!(fenced.status, RuntimeJobStatus::Running);
    assert!(fenced.input.get("cancellation_requested").is_some());
    assert_eq!(
        store
            .get_command(&command_id)
            .await?
            .expect("legacy terminal command should remain auditable")
            .status,
        WorkflowCommandStatus::Cancelled
    );
    Ok(())
}

#[tokio::test]
async fn remote_eval_deregistration_serializes_with_terminal_transition() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let instance = issue_instance("implementing").with_id("terminal-deregister-eval-race");
    store
        .force_upsert_lifecycle_state_for_test(&instance)
        .await?;
    let command = WorkflowCommand::enqueue_activity(
        "implement_issue",
        "terminal-deregister-eval-command",
    );
    let command_id = store.enqueue_command(&instance.id, None, &command).await?;
    let job = store
        .enqueue_runtime_job(
            &command_id,
            RuntimeKind::RemoteHost,
            "eval-host",
            json!({
                "activity": "implement_issue",
                "eval": {"timeout_secs": 60}
            }),
        )
        .await?;
    let lease_expires_at = Utc::now() + Duration::minutes(5);
    store
        .claim_next_runtime_job_for_runtime_kind(
            RuntimeKind::RemoteHost,
            "terminal-deregister-host",
            lease_expires_at,
        )
        .await?
        .expect("remote eval should be claimed before the race");

    let result = ActivityResult::succeeded(
        "implement_issue",
        "The issue closed while its runtime host was deregistering.",
    )
    .with_signal(ActivitySignal::new(
        "IssueClosed",
        json!({
            "issue_number": 123,
            "state": "closed",
            "issue_url": "https://github.com/owner/repo/issues/123"
        }),
    ))
    .with_artifact(crate::runtime::completion_evidence::verified_issue_state_for_test(123));

    let deregister =
        store.revoke_remote_host_runtime_job_leases("terminal-deregister-host", Utc::now());
    let terminal = store.commit_parent_runtime_completion(
        &instance.id,
        "terminal-deregister-runtime",
        json!({
            "command_id": "terminal-deregister-completed-command",
            "runtime_job_id": "terminal-deregister-completed-job",
            "activity_result": result,
        }),
    );
    let (revoked, terminal) = tokio::join!(deregister, terminal);
    assert_eq!(revoked?, 0, "remote eval cleanup ownership must be preserved");
    let record = terminal?.expect("closed issue should produce a terminal decision");
    assert!(record.accepted);
    assert_eq!(record.decision.next_state, "done");

    let cancelling = store
        .get_runtime_job(&job.id)
        .await?
        .expect("cancelling remote eval should remain auditable");
    assert_eq!(cancelling.status, RuntimeJobStatus::Running);
    assert!(cancelling.lease.is_some());
    assert!(cancelling.input.get("cancellation_requested").is_some());
    assert_eq!(
        store
            .get_command(&command_id)
            .await?
            .expect("terminal command should remain auditable")
            .status,
        WorkflowCommandStatus::Cancelled
    );
    Ok(())
}
