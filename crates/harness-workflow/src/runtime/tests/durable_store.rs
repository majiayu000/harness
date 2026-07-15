#[tokio::test]
async fn durable_store_persists_workflow_runtime_bus_contract() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;

    let definition = WorkflowDefinition::new("github_issue_pr", 1, "GitHub Issue PR");
    store.upsert_definition(&definition).await?;

    let instance = issue_instance("implementing");
    store.upsert_instance(&instance).await?;
    let loaded = store
        .get_instance(&instance.id)
        .await?
        .expect("workflow instance should load");
    assert_eq!(loaded.id, instance.id);
    assert_eq!(loaded.state, "implementing");

    let first = store
        .append_event(
            &instance.id,
            "PlanIssueRaised",
            "runtime_job",
            json!({ "detail": "missing migration path" }),
        )
        .await?;
    let second = store
        .append_event(
            &instance.id,
            "PolicyDecisionRequested",
            "workflow_controller",
            json!({ "reason": "plan issue needs policy" }),
        )
        .await?;
    assert_eq!(first.sequence, 1);
    assert_eq!(second.sequence, 2);
    assert_eq!(store.events_for(&instance.id).await?.len(), 2);

    let decision = WorkflowDecision::new(
        instance.id.clone(),
        "implementing",
        "run_replan",
        "replanning",
        "Replan before continuing implementation.",
    )
    .with_command(WorkflowCommand::enqueue_activity(
        "replan_issue",
        "issue-123-replan-1",
    ));
    let record = WorkflowDecisionRecord::accepted(decision.clone(), Some(first.id.clone()));
    store.record_decision(&record).await?;

    let command_id = store
        .enqueue_command(&instance.id, Some(&record.id), &decision.commands[0])
        .await?;
    let duplicate_command_id = store
        .enqueue_command(&instance.id, Some(&record.id), &decision.commands[0])
        .await?;
    assert_eq!(
        command_id, duplicate_command_id,
        "dedupe key should make command enqueue idempotent"
    );

    let job = store
        .enqueue_runtime_job(
            &command_id,
            RuntimeKind::CodexJsonrpc,
            "codex-high",
            json!({ "prompt_packet": { "workflow_id": instance.id } }),
        )
        .await?;
    assert_eq!(
        store
            .runtime_job_count_by_status(RuntimeJobStatus::Pending)
            .await?,
        1
    );

    let claimed = store
        .claim_next_runtime_job("runtime-1", Utc::now() + Duration::minutes(5))
        .await?
        .expect("runtime job should be claimable");
    assert_eq!(claimed.id, job.id);
    assert_eq!(claimed.status, RuntimeJobStatus::Running);

    let runtime_first = store
        .record_runtime_event(&claimed.id, "TurnStarted", json!({ "runtime": "codex" }))
        .await?;
    let runtime_second = store
        .record_runtime_event(
            &claimed.id,
            "ActivityResultReady",
            json!({ "activity": "replan_issue" }),
        )
        .await?;
    assert_eq!(runtime_first.sequence, 1);
    assert_eq!(runtime_second.sequence, 2);
    assert_eq!(store.runtime_events_for(&claimed.id).await?.len(), 2);

    let result = ActivityResult::succeeded("replan_issue", "Replan completed.")
        .with_signal(ActivitySignal::new("ReplanCompleted", json!({})));
    let lease_expires_at = claimed
        .lease
        .as_ref()
        .expect("lease should exist")
        .expires_at;
    let completed = store
        .complete_runtime_job_if_owned(&claimed.id, "runtime-1", lease_expires_at, &result)
        .await?
        .expect("current owner completion should be accepted");
    assert_eq!(completed.status, RuntimeJobStatus::Succeeded);
    assert_eq!(
        store
            .get_runtime_job(&claimed.id)
            .await?
            .expect("completed job should load")
            .status,
        RuntimeJobStatus::Succeeded
    );

    Ok(())
}

#[tokio::test]
async fn durable_store_apply_decision_transition_can_create_initial_instance() -> anyhow::Result<()>
{
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;

    let initial = project_issue_instance("/project-a", 123, "implementing");
    let decision = WorkflowDecision::new(
        &initial.id,
        "implementing",
        "bind_pr",
        "pr_open",
        "implementation produced a pull request for the issue workflow",
    )
    .with_command(WorkflowCommand::bind_pr(
        77,
        "https://github.com/owner/repo/pull/77",
        "pr-detected:task-1:77",
    ));
    let mut final_instance = initial.clone();
    final_instance.state = "pr_open".to_string();
    final_instance.version = final_instance.version.saturating_add(1);
    final_instance.data = json!({
        "project_id": "/project-a",
        "issue_number": 123,
        "pr_number": 77,
        "pr_url": "https://github.com/owner/repo/pull/77",
        "last_decision": "bind_pr",
    });

    let record = store
        .apply_decision_transition(WorkflowDecisionTransition {
            expected_state: "implementing",
            create_if_missing: Some(&initial),
            event_type: "PrDetected",
            source: "workflow-runtime-test",
            payload: json!({
                "issue_number": 123,
                "pr_number": 77,
                "pr_url": "https://github.com/owner/repo/pull/77",
            }),
            decision: &decision,
            final_instance: &final_instance,
            command_status: WorkflowCommandStatus::Pending,
        })
        .await?
        .expect("missing initial instance should be created inside the transition");

    assert!(record.accepted);
    assert!(record.event_id.is_some());
    let loaded = store
        .get_instance(&initial.id)
        .await?
        .expect("transition should persist final instance");
    assert_eq!(loaded.state, "pr_open");
    assert_eq!(loaded.data["pr_number"], 77);
    assert_eq!(store.events_for(&initial.id).await?.len(), 1);
    assert_eq!(store.decisions_for(&initial.id).await?.len(), 1);
    let commands = store.commands_for(&initial.id).await?;
    assert_eq!(commands.len(), 1);
    assert_eq!(commands[0].decision_id.as_deref(), Some(record.id.as_str()));
    Ok(())
}

#[tokio::test]
async fn durable_store_apply_decision_transition_does_not_rewind_existing_instance(
) -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;

    let initial = project_issue_instance("/project-a", 124, "implementing");
    let mut existing = initial.clone();
    existing.state = "awaiting_feedback".to_string();
    existing.data = json!({
        "project_id": "/project-a",
        "issue_number": 124,
        "pr_number": 78,
        "last_decision": "record_feedback",
    });
    store.upsert_instance(&existing).await?;
    let decision = WorkflowDecision::new(
        &initial.id,
        "implementing",
        "bind_pr",
        "pr_open",
        "implementation produced a pull request for the issue workflow",
    )
    .with_command(WorkflowCommand::bind_pr(
        79,
        "https://github.com/owner/repo/pull/79",
        "pr-detected:task-2:79",
    ));
    let mut final_instance = initial.clone();
    final_instance.state = "pr_open".to_string();
    final_instance.version = final_instance.version.saturating_add(1);
    final_instance.data = json!({
        "project_id": "/project-a",
        "issue_number": 124,
        "pr_number": 79,
        "pr_url": "https://github.com/owner/repo/pull/79",
        "last_decision": "bind_pr",
    });

    let record = store
        .apply_decision_transition(WorkflowDecisionTransition {
            expected_state: "implementing",
            create_if_missing: Some(&initial),
            event_type: "PrDetected",
            source: "workflow-runtime-test",
            payload: json!({
                "issue_number": 124,
                "pr_number": 79,
                "pr_url": "https://github.com/owner/repo/pull/79",
            }),
            decision: &decision,
            final_instance: &final_instance,
            command_status: WorkflowCommandStatus::Pending,
        })
        .await?;

    assert!(record.is_none());
    let loaded = store
        .get_instance(&initial.id)
        .await?
        .expect("existing instance should remain visible");
    assert_eq!(loaded.state, "awaiting_feedback");
    assert_eq!(loaded.data["pr_number"], 78);
    assert!(store.events_for(&initial.id).await?.is_empty());
    assert!(store.decisions_for(&initial.id).await?.is_empty());
    Ok(())
}

#[tokio::test]
async fn durable_store_lists_workflow_runtime_tree_inputs() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;

    let parent = quality_gate_instance("checking").with_data(json!({
        "project_id": "/project-a",
        "repo": "owner/repo",
    }));
    let child =
        project_issue_instance("/project-a", 123, "replanning").with_parent(parent.id.clone());
    let other_project = project_issue_instance("/project-b", 456, "implementing");
    store.upsert_instance(&parent).await?;
    store.upsert_instance(&child).await?;
    store.upsert_instance(&other_project).await?;
    let event = store
        .append_event(
            &child.id,
            "PlanIssueRaised",
            "workflow-runtime-test",
            json!({ "issue_number": 123 }),
        )
        .await?;
    let rejected_decision = WorkflowDecision::new(
        child.id.clone(),
        "replanning",
        "run_replan",
        "replanning",
        "Replan requested after the budget was exhausted.",
    );
    let rejected = WorkflowDecisionRecord::rejected(
        rejected_decision,
        Some(event.id),
        "replan limit exhausted",
    );
    store.record_decision(&rejected).await?;

    let command = WorkflowCommand::enqueue_activity("replan_issue", "issue-123-replan-2");
    let command_id = store
        .enqueue_command(&child.id, Some(&rejected.id), &command)
        .await?;
    let job = store
        .enqueue_runtime_job(
            &command_id,
            RuntimeKind::CodexJsonrpc,
            "codex-high",
            json!({ "workflow_id": child.id }),
        )
        .await?;

    let listed = store.list_instances(Some("/project-a"), 25).await?;
    assert_eq!(listed.len(), 2);
    assert!(listed.iter().all(|instance| {
        instance
            .data
            .get("project_id")
            .and_then(|value| value.as_str())
            == Some("/project-a")
    }));

    let decisions = store.decisions_for(&child.id).await?;
    assert_eq!(decisions.len(), 1);
    assert!(!decisions[0].accepted);
    assert_eq!(
        decisions[0].rejection_reason.as_deref(),
        Some("replan limit exhausted")
    );

    let commands: Vec<WorkflowCommandRecord> = store.commands_for(&child.id).await?;
    assert_eq!(commands.len(), 1);
    assert_eq!(commands[0].id, command_id);
    assert_eq!(commands[0].command.activity_name(), Some("replan_issue"));

    let jobs = store.runtime_jobs_for_command(&command_id).await?;
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].id, job.id);
    assert_eq!(jobs[0].status, RuntimeJobStatus::Pending);

    Ok(())
}

#[tokio::test]
async fn durable_store_lists_nonterminal_instances_by_definition() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let active = project_issue_instance("/project-a", 123, "implementing");
    let queued = project_issue_instance("/project-a", 124, "ready_to_merge");
    let terminal = project_issue_instance("/project-a", 125, "done");
    let other_definition = prompt_task_instance("implementing").with_data(json!({
        "project_id": "/project-a",
        "repo": "owner/repo",
    }));
    let other_project = project_issue_instance("/project-b", 126, "implementing");
    store.upsert_instance(&active).await?;
    store.upsert_instance(&queued).await?;
    store.upsert_instance(&terminal).await?;
    store.upsert_instance(&other_definition).await?;
    store.upsert_instance(&other_project).await?;

    let listed = store
        .list_nonterminal_instances_by_definition(
            GITHUB_ISSUE_PR_DEFINITION_ID,
            Some("/project-a"),
            None,
        )
        .await?;
    let ids: std::collections::HashSet<_> =
        listed.into_iter().map(|instance| instance.id).collect();

    assert!(ids.contains(&active.id));
    assert!(ids.contains(&queued.id));
    assert!(!ids.contains(&terminal.id));
    assert!(!ids.contains(&other_definition.id));
    assert!(!ids.contains(&other_project.id));
    Ok(())
}

#[tokio::test]
async fn driverless_completion_is_rejected_atomically() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let instance = issue_instance("replanning")
        .with_id("issue-driverless-completion")
        .with_data(json!({ "marker": "must-remain" }));
    store.upsert_instance(&instance).await?;

    let stale_command = WorkflowCommand::enqueue_activity(
        "implement_issue",
        "stale-active-work-from-an-older-decision",
    );
    let stale_command_id = store
        .enqueue_command(&instance.id, None, &stale_command)
        .await?;
    let stale_job = store
        .enqueue_runtime_job(
            &stale_command_id,
            RuntimeKind::CodexJsonrpc,
            "codex-high",
            json!({ "workflow_id": instance.id, "activity": "implement_issue" }),
        )
        .await?;
    let driverless = WorkflowDecision::new(
        &instance.id,
        "replanning",
        "resume_implementation_after_replan",
        "implementing",
        "Resume implementation without durable work.",
    )
    .with_command(WorkflowCommand::wait(
        "Waiting is not a progress driver.",
        "driverless-completion-wait",
    ));

    let first_completion = store.commit_runtime_completion_decision_for_test(
        &instance.id,
        "runtime-1",
        json!({
            "command_id": "completed-command",
            "runtime_job_id": "completed-job",
        }),
        &driverless,
    );
    let replayed_completion = store.commit_runtime_completion_decision_for_test(
        &instance.id,
        "runtime-2",
        json!({
            "command_id": "replayed-command",
            "runtime_job_id": "replayed-job",
        }),
        &driverless,
    );
    let (first_record, replayed_record) = tokio::join!(first_completion, replayed_completion);
    let record = first_record?
        .expect("completion should persist a rejected decision");
    let replayed_record = replayed_record?
        .expect("replayed completion should persist a rejected decision");

    for rejected in [&record, &replayed_record] {
        assert!(!rejected.accepted);
        assert_eq!(rejected.decision, driverless);
        assert!(rejected
            .rejection_reason
            .as_deref()
            .is_some_and(|reason| reason.starts_with("ProgressDriverMissing:")));
    }
    let after = store
        .get_instance(&instance.id)
        .await?
        .expect("workflow instance should remain visible");
    assert_eq!(after.state, instance.state);
    assert_eq!(after.version, instance.version);
    assert_eq!(after.data, instance.data);

    let decisions = store.decisions_for(&instance.id).await?;
    assert_eq!(decisions.len(), 2);
    assert!(decisions.iter().all(|decision| !decision.accepted));
    assert!(decisions
        .iter()
        .any(|decision| decision.id == record.id));
    assert!(decisions
        .iter()
        .any(|decision| decision.id == replayed_record.id));
    assert_eq!(store.events_for(&instance.id).await?.len(), 2);
    let commands = store.commands_for(&instance.id).await?;
    assert_eq!(commands.len(), 1);
    assert_eq!(commands[0].id, stale_command_id);
    assert_eq!(commands[0].decision_id, None);
    let jobs = store.runtime_jobs_for_command(&stale_command_id).await?;
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].id, stale_job.id);
    assert_eq!(jobs[0].status, RuntimeJobStatus::Pending);
    let counts = store
        .detail_counts_for_workflows(std::slice::from_ref(&instance.id))
        .await?
        .remove(&instance.id)
        .expect("workflow detail counts should be present");
    assert_eq!(counts.rejected_decision_count, 2);
    assert_eq!(counts.command_count, 1);
    assert_eq!(counts.runtime_job_count, 1);
    Ok(())
}

#[tokio::test]
async fn runtime_activity_completion_fences_concurrent_driverless_replay() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let instance = issue_instance("replanning")
        .with_id("issue-driverless-production-completion")
        .with_data(json!({ "marker": "must-remain" }));
    store.upsert_instance(&instance).await?;

    let state_entry = WorkflowDecision::new(
        &instance.id,
        "implementing",
        "run_replan",
        "replanning",
        "Replan before resuming implementation.",
    )
    .with_command(WorkflowCommand::enqueue_activity(
        "replan_issue",
        "driverless-production-replan",
    ));
    let state_entry_record = WorkflowDecisionRecord::accepted(state_entry.clone(), None);
    store.record_decision(&state_entry_record).await?;
    let command_id = store
        .enqueue_command(
            &instance.id,
            Some(&state_entry_record.id),
            &state_entry.commands[0],
        )
        .await?;
    let job = store
        .enqueue_runtime_job(
            &command_id,
            RuntimeKind::CodexJsonrpc,
            "codex-high",
            json!({ "workflow_id": instance.id, "activity": "replan_issue" }),
        )
        .await?;
    let claimed = store
        .claim_next_runtime_job("runtime-1", Utc::now() + Duration::minutes(5))
        .await?
        .expect("runtime job should be claimable");
    assert_eq!(claimed.id, job.id);
    let lease_expires_at = claimed
        .lease
        .as_ref()
        .expect("claimed runtime job should have a lease")
        .expires_at;

    let driverless = WorkflowDecision::new(
        &instance.id,
        "replanning",
        "resume_implementation_after_replan",
        "implementing",
        "Resume implementation without durable work.",
    )
    .with_command(WorkflowCommand::wait(
        "Waiting is not a progress driver.",
        "driverless-production-wait",
    ));
    let result = ActivityResult::succeeded("replan_issue", "Replan completed.").with_artifact(
        ActivityArtifact::new(
            "workflow_decision",
            serde_json::to_value(&driverless)?,
        ),
    );

    let first = store.commit_runtime_activity_completion_if_owned(
        &claimed.id,
        "runtime-1",
        lease_expires_at,
        &result,
    );
    let replay = store.commit_runtime_activity_completion_if_owned(
        &claimed.id,
        "runtime-1",
        lease_expires_at,
        &result,
    );
    let (first, replay) = tokio::join!(first, replay);
    let first = first?;
    let replay = replay?;
    assert!(
        first.is_some() ^ replay.is_some(),
        "the runtime job lease must admit exactly one completion"
    );
    let winner = first.or(replay).expect("one completion should win");
    let rejected = winner
        .decision
        .expect("the winning completion should persist a decision");
    assert!(!rejected.accepted);
    assert_eq!(rejected.decision, driverless);
    assert!(rejected
        .rejection_reason
        .as_deref()
        .is_some_and(|reason| reason.starts_with("ProgressDriverMissing:")));

    let after = store
        .get_instance(&instance.id)
        .await?
        .expect("workflow instance should remain visible");
    assert_eq!(after.state, instance.state);
    assert_eq!(after.version, instance.version);
    assert_eq!(after.data, instance.data);
    let commands = store.commands_for(&instance.id).await?;
    assert_eq!(commands.len(), 1);
    assert_eq!(commands[0].id, command_id);
    assert_eq!(
        commands[0].decision_id.as_deref(),
        Some(state_entry_record.id.as_str())
    );
    assert_eq!(commands[0].status, WorkflowCommandStatus::Completed);
    let jobs = store.runtime_jobs_for_command(&command_id).await?;
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].status, RuntimeJobStatus::Succeeded);
    assert_eq!(store.events_for(&instance.id).await?.len(), 1);
    let decisions = store.decisions_for(&instance.id).await?;
    assert_eq!(decisions.len(), 2);
    assert_eq!(decisions.iter().filter(|decision| decision.accepted).count(), 1);
    assert_eq!(
        decisions.iter().filter(|decision| !decision.accepted).count(),
        1
    );
    Ok(())
}
