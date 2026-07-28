#[tokio::test]
async fn prompt_continuation_completion_persists_context_and_dedupes_attempt_command(
) -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let policy = PromptContinuationPolicy {
        max_attempts: 4,
        attempt_delay_secs: 0,
        active_states: std::collections::BTreeSet::from(["In Progress".to_string()]),
        no_progress_limit: 3,
    };
    let instance = WorkflowInstance::new(
        PROMPT_TASK_DEFINITION_ID,
        1,
        "implementing",
        WorkflowSubject::new("prompt", "TEAM-123"),
    )
    .with_id("prompt-continuation-store")
    .with_data(json!({
        "prompt_ref": "prompt-ref-store",
        "continuation": PromptContinuationState::initial(&policy),
    }));
    store.upsert_instance(&instance).await?;
    let result = ActivityResult::succeeded(
        PROMPT_TASK_IMPLEMENT_ACTIVITY,
        "Created the implementation branch.",
    )
    .with_signal(ActivitySignal::new(
        "external_state",
        json!({ "state": "In Progress", "subject": "TEAM-123" }),
    ));
    let payload = json!({
        "command_id": "attempt-1-command",
        "runtime_job_id": "attempt-1-job",
        "activity_result": result,
    });
    let event = WorkflowEvent::new(
        &instance.id,
        1,
        RUNTIME_JOB_COMPLETED_EVENT,
        "runtime-1",
    )
    .with_payload(payload.clone());
    let decision = reduce_runtime_job_completed(&instance, &event)?
        .expect("active continuation should produce a decision");

    let first = store
        .commit_runtime_completion_decision_for_test(
            &instance.id,
            "runtime-1",
            payload.clone(),
            &decision,
        )
        .await?
        .expect("first completion should commit");
    assert!(first.accepted);
    let after = store
        .get_instance(&instance.id)
        .await?
        .expect("continuation instance should reload");
    assert_eq!(after.data["continuation"]["attempt"], 2);
    assert_eq!(
        after.data["continuation"]["last_summary"],
        "Created the implementation branch."
    );
    assert!(after.data["continuation"]["last_progress_fingerprint"]
        .as_str()
        .is_some());

    let replay = store
        .commit_runtime_completion_decision_for_test(
            &instance.id,
            "runtime-1",
            payload,
            &decision,
        )
        .await?
        .expect("replayed completion should remain idempotent");
    assert!(replay.accepted);
    let commands = store.commands_for(&instance.id).await?;
    assert_eq!(commands.len(), 1);
    assert_eq!(
        commands[0].command.dedupe_key,
        "prompt-task:prompt-continuation-store:attempt:2"
    );
    assert_eq!(commands[0].command.command["prompt_ref"], "prompt-ref-store");
    Ok(())
}

#[tokio::test]
async fn settled_prompt_missing_completion_evidence_persists_observed_context(
) -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let policy = PromptContinuationPolicy {
        max_attempts: 4,
        attempt_delay_secs: 0,
        active_states: std::collections::BTreeSet::from(["In Progress".to_string()]),
        no_progress_limit: 3,
    };
    let instance = WorkflowInstance::new(
        PROMPT_TASK_DEFINITION_ID,
        1,
        "implementing",
        WorkflowSubject::new("prompt", "TEAM-124"),
    )
    .with_id("prompt-continuation-missing-evidence")
    .with_data(json!({
        "prompt_ref": "prompt-ref-missing-evidence",
        "continuation": PromptContinuationState::initial(&policy),
    }));
    store.upsert_instance(&instance).await?;
    let result = ActivityResult::succeeded(
        PROMPT_TASK_IMPLEMENT_ACTIVITY,
        "External work settled without completion evidence.",
    )
    .with_signal(ActivitySignal::new(
        "external_state",
        json!({ "state": "Done", "subject": "TEAM-124" }),
    ));
    let payload = json!({
        "command_id": "settled-command",
        "runtime_job_id": "settled-job",
        "activity_result": result,
    });
    let event = WorkflowEvent::new(
        &instance.id,
        1,
        RUNTIME_JOB_COMPLETED_EVENT,
        "runtime-1",
    )
    .with_payload(payload.clone());
    let decision = reduce_runtime_job_completed(&instance, &event)?
        .expect("settled result without evidence should produce a blocked decision");

    assert_eq!(decision.decision, "prompt_completion_evidence_missing");
    let committed = store
        .commit_runtime_completion_decision_for_test(
            &instance.id,
            "runtime-1",
            payload,
            &decision,
        )
        .await?
        .expect("blocked completion should commit");
    assert!(committed.accepted);

    let after = store
        .get_instance(&instance.id)
        .await?
        .expect("blocked continuation should reload");
    assert_eq!(after.state, "blocked");
    assert_eq!(after.data["continuation"]["attempt"], 1);
    assert_eq!(
        after.data["continuation"]["last_external_state"],
        "Done"
    );
    assert_eq!(
        after.data["continuation"]["last_summary"],
        "External work settled without completion evidence."
    );
    Ok(())
}

#[tokio::test]
async fn single_shot_prompt_missing_completion_evidence_still_commits() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let instance = WorkflowInstance::new(
        PROMPT_TASK_DEFINITION_ID,
        1,
        "implementing",
        WorkflowSubject::new("prompt", "single-shot"),
    )
    .with_id("prompt-single-shot-missing-evidence")
    .with_data(json!({ "prompt_ref": "prompt-ref-single-shot" }));
    store.upsert_instance(&instance).await?;
    let result = ActivityResult::succeeded(
        PROMPT_TASK_IMPLEMENT_ACTIVITY,
        "Single-shot work omitted completion evidence.",
    );
    let payload = json!({
        "command_id": "single-shot-command",
        "runtime_job_id": "single-shot-job",
        "activity_result": result,
    });
    let event = WorkflowEvent::new(
        &instance.id,
        1,
        RUNTIME_JOB_COMPLETED_EVENT,
        "runtime-1",
    )
    .with_payload(payload.clone());
    let decision = reduce_runtime_job_completed(&instance, &event)?
        .expect("single-shot result without evidence should produce a blocked decision");

    assert_eq!(decision.decision, "prompt_completion_evidence_missing");
    let committed = store
        .commit_runtime_completion_decision_for_test(
            &instance.id,
            "runtime-1",
            payload,
            &decision,
        )
        .await?
        .expect("single-shot blocked completion should commit");
    assert!(committed.accepted);
    let after = store
        .get_instance(&instance.id)
        .await?
        .expect("single-shot instance should reload");
    assert_eq!(after.state, "blocked");
    assert!(after.data.get("continuation").is_none());
    Ok(())
}
