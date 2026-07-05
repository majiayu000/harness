#[test]
fn in_memory_bus_passes_workflow_commands_to_runtime_jobs() {
    let mut bus = InMemoryWorkflowBus::default();
    let instance = issue_instance("pr_open");
    bus.insert_instance(instance.clone());

    let event = bus.append_event(
        instance.id.clone(),
        "PrReviewUpdated",
        "github_webhook",
        json!({ "pr": 77 }),
    );
    let decision = WorkflowDecision::new(
        instance.id.clone(),
        "pr_open",
        "sweep_feedback",
        "awaiting_feedback",
        "A PR update should trigger feedback sweep.",
    )
    .with_command(WorkflowCommand::start_child_workflow(
        "pr_feedback",
        "pr:77",
        "issue-123-pr-77-feedback",
    ));
    let record = WorkflowDecisionRecord::accepted(decision.clone(), Some(event.id.clone()));
    bus.record_decision(record);

    let command_id = bus.enqueue_command(decision.commands[0].clone());
    let job = bus.enqueue_runtime_job(
        command_id.clone(),
        RuntimeKind::CodexJsonrpc,
        "codex-feedback",
        json!({ "workflow_id": instance.id, "subject": "pr:77" }),
    );

    assert_eq!(
        bus.command(&command_id).expect("command exists").dedupe_key,
        "issue-123-pr-77-feedback"
    );

    let claimed = bus
        .claim_next_runtime_job("runtime-1", Utc::now() + Duration::minutes(5))
        .expect("runtime job should be claimable");
    assert_eq!(claimed.id, job.id);
    assert_eq!(claimed.status, RuntimeJobStatus::Running);

    let first_event = bus.record_runtime_event(
        claimed.id.clone(),
        "TurnStarted",
        json!({ "runtime": "codex_jsonrpc" }),
    );
    let second_event = bus.record_runtime_event(
        claimed.id.clone(),
        "ActivityResultReady",
        json!({ "activity": "sweep_feedback" }),
    );
    assert_eq!(first_event.sequence, 1);
    assert_eq!(second_event.sequence, 2);

    let result = ActivityResult::succeeded("sweep_feedback", "Found actionable feedback.")
        .with_signal(ActivitySignal::new("FeedbackFound", json!({ "count": 1 })));
    let completed = bus
        .complete_runtime_job(&claimed.id, &result)
        .expect("runtime job should complete");
    assert_eq!(completed.status, RuntimeJobStatus::Succeeded);
    assert_eq!(bus.runtime_events_for(&claimed.id).len(), 2);
}
