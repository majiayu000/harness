#[tokio::test]
async fn driverless_progress_reports_only_unowned_command_driven_instances() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }
    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;

    let driverless = driverless_progress_instance(&store, "driverless", "implementing").await?;
    let driverless_decision = accepted_state_entry(&store, &driverless, "implementing").await?;

    let active = driverless_progress_instance(&store, "active", "implementing").await?;
    let active_decision = accepted_state_entry(&store, &active, "implementing").await?;
    let child_driver = WorkflowCommand::start_child_workflow(
        PR_FEEDBACK_DEFINITION_ID,
        "issue:123:pr-feedback",
        "active-child-driver",
    );
    store
        .enqueue_command_with_status(
            &active.id,
            Some(&active_decision.id),
            &child_driver,
            WorkflowCommandStatus::Dispatching,
        )
        .await?;

    let deferred = driverless_progress_instance(&store, "deferred", "implementing").await?;
    let deferred_decision = accepted_state_entry(&store, &deferred, "implementing").await?;
    insert_driver_command(
        &store,
        &deferred.id,
        Some(&deferred_decision.id),
        "deferred-driver",
        WorkflowCommandStatus::Deferred,
    )
    .await?;

    let unfinished_job =
        driverless_progress_instance(&store, "unfinished-job", "implementing").await?;
    let unfinished_decision =
        accepted_state_entry(&store, &unfinished_job, "implementing").await?;
    let unfinished_command = insert_driver_command(
        &store,
        &unfinished_job.id,
        Some(&unfinished_decision.id),
        "unfinished-job-driver",
        WorkflowCommandStatus::Pending,
    )
    .await?;
    store
        .enqueue_runtime_job(
            &unfinished_command,
            RuntimeKind::CodexExec,
            "default",
            json!({ "activity": "implement_issue" }),
        )
        .await?;
    sqlx::query("UPDATE workflow_commands SET status = 'completed' WHERE id = $1")
        .bind(&unfinished_command)
        .execute(store.pool())
        .await?;

    let stale = driverless_progress_instance(&store, "stale", "implementing").await?;
    let stale_decision =
        accepted_state_entry_with_id(&store, &stale, "planning", "zzzz-older-decision").await?;
    insert_driver_command(
        &store,
        &stale.id,
        Some(&stale_decision.id),
        "stale-driver",
        WorkflowCommandStatus::Pending,
    )
    .await?;
    let stale_current =
        accepted_state_entry_with_id(&store, &stale, "implementing", "aaaa-newer-decision").await?;

    let dedupe_collision =
        driverless_progress_instance(&store, "dedupe-collision", "implementing").await?;
    let dedupe_older = accepted_state_entry(&store, &dedupe_collision, "planning").await?;
    insert_driver_command(
        &store,
        &dedupe_collision.id,
        Some(&dedupe_older.id),
        "shared-transition-dedupe-key",
        WorkflowCommandStatus::Pending,
    )
    .await?;
    accepted_state_entry(&store, &dedupe_collision, "implementing").await?;

    let unrelated =
        driverless_progress_instance(&store, "unrelated-decision", "implementing").await?;
    accepted_state_entry(&store, &unrelated, "implementing").await?;
    insert_driver_command(
        &store,
        &unrelated.id,
        Some(&active_decision.id),
        "unrelated-decision-driver",
        WorkflowCommandStatus::Pending,
    )
    .await?;

    let terminal_work =
        driverless_progress_instance(&store, "terminal-work", "implementing").await?;
    let terminal_decision =
        accepted_state_entry(&store, &terminal_work, "implementing").await?;
    let terminal_command = insert_driver_command(
        &store,
        &terminal_work.id,
        Some(&terminal_decision.id),
        "terminal-driver",
        WorkflowCommandStatus::Pending,
    )
    .await?;
    let terminal_job = store
        .enqueue_runtime_job(
            &terminal_command,
            RuntimeKind::CodexExec,
            "default",
            json!({ "activity": "implement_issue" }),
        )
        .await?;
    sqlx::query("UPDATE workflow_commands SET status = 'completed' WHERE id = $1")
        .bind(&terminal_command)
        .execute(store.pool())
        .await?;
    sqlx::query("UPDATE runtime_jobs SET status = 'succeeded' WHERE id = $1")
        .bind(&terminal_job.id)
        .execute(store.pool())
        .await?;

    let null_provenance =
        driverless_progress_instance(&store, "null-provenance", "implementing").await?;
    accepted_state_entry(&store, &null_provenance, "implementing").await?;
    insert_driver_command(
        &store,
        &null_provenance.id,
        None,
        "null-driver",
        WorkflowCommandStatus::Pending,
    )
    .await?;

    let rejected = driverless_progress_instance(&store, "rejected", "implementing").await?;
    let rejected_event = store
        .append_event(&rejected.id, "test", "test", json!({}))
        .await?;
    let rejected_record = WorkflowDecisionRecord::rejected(
        WorkflowDecision::new(
            &rejected.id,
            "planning",
            "reject",
            "implementing",
            "test rejection",
        ),
        Some(rejected_event.id),
        "test rejection",
    );
    store.record_decision(&rejected_record).await?;
    insert_driver_command(
        &store,
        &rejected.id,
        Some(&rejected_record.id),
        "rejected-driver",
        WorkflowCommandStatus::Pending,
    )
    .await?;

    let non_command =
        driverless_progress_instance(&store, "external-wait", "awaiting_dependencies").await?;
    let terminal = driverless_progress_instance(&store, "terminal", "done").await?;

    let before_counts = runtime_history_counts(&store).await?;
    let rows = store.list_driverless_progress_instances(500).await?;
    let after_counts = runtime_history_counts(&store).await?;
    assert_eq!(before_counts, after_counts, "diagnostic must be read-only");

    let by_id: std::collections::HashMap<_, _> = rows
        .iter()
        .map(|row| (row.workflow_id.as_str(), row))
        .collect();
    assert_eq!(
        by_id[driverless.id.as_str()].provenance_status,
        super::store::DriverlessProgressProvenanceStatus::Established
    );
    assert_eq!(
        by_id[stale.id.as_str()].provenance_status,
        super::store::DriverlessProgressProvenanceStatus::Established
    );
    assert_eq!(
        by_id[dedupe_collision.id.as_str()].provenance_status,
        super::store::DriverlessProgressProvenanceStatus::Established
    );
    assert_eq!(
        by_id[unrelated.id.as_str()].provenance_status,
        super::store::DriverlessProgressProvenanceStatus::Established
    );
    assert_eq!(
        by_id[terminal_work.id.as_str()].provenance_status,
        super::store::DriverlessProgressProvenanceStatus::Established
    );
    assert_eq!(
        by_id[null_provenance.id.as_str()].provenance_status,
        super::store::DriverlessProgressProvenanceStatus::Established
    );
    assert_eq!(
        by_id[rejected.id.as_str()].provenance_status,
        super::store::DriverlessProgressProvenanceStatus::MissingStateEntryProvenance
    );
    assert!(!by_id.contains_key(active.id.as_str()));
    assert!(!by_id.contains_key(deferred.id.as_str()));
    assert!(!by_id.contains_key(unfinished_job.id.as_str()));
    assert!(!by_id.contains_key(non_command.id.as_str()));
    assert!(!by_id.contains_key(terminal.id.as_str()));
    assert_eq!(driverless_decision.workflow_id, driverless.id);
    assert_eq!(stale_current.workflow_id, stale.id);
    assert!(rows.iter().all(|row| row.age_secs < 60));
    Ok(())
}

#[tokio::test]
async fn driverless_progress_classifies_missing_and_ambiguous_provenance() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }
    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;

    let missing = driverless_progress_instance(&store, "missing", "implementing").await?;
    let mismatched = driverless_progress_instance(&store, "mismatched", "implementing").await?;
    accepted_state_entry(&store, &mismatched, "planning").await?;

    let equal = driverless_progress_instance(&store, "equal", "implementing").await?;
    let equal_event = store
        .append_event(&equal.id, "test", "test", json!({}))
        .await?;
    for decision_name in ["equal-a", "equal-b"] {
        let mut record = WorkflowDecisionRecord::accepted(
            WorkflowDecision::new(
                &equal.id,
                "planning",
                decision_name,
                "implementing",
                "equal sequence fixture",
            ),
            Some(equal_event.id.clone()),
        );
        record.id = format!("{decision_name}-decision");
        store.record_decision(&record).await?;
    }

    let unsequenced =
        driverless_progress_instance(&store, "unsequenced", "implementing").await?;
    accepted_state_entry(&store, &unsequenced, "implementing").await?;
    let mut unsequenced_record = WorkflowDecisionRecord::accepted(
        WorkflowDecision::new(
            &unsequenced.id,
            "planning",
            "unsequenced",
            "implementing",
            "missing event sequence fixture",
        ),
        None,
    );
    unsequenced_record.id = "unsequenced-decision".to_string();
    store.record_decision(&unsequenced_record).await?;

    let rows = store.list_driverless_progress_instances(500).await?;
    let by_id: std::collections::HashMap<_, _> = rows
        .iter()
        .map(|row| (row.workflow_id.as_str(), row))
        .collect();
    for workflow_id in [&missing.id, &mismatched.id] {
        assert_eq!(
            by_id[workflow_id.as_str()].provenance_status,
            super::store::DriverlessProgressProvenanceStatus::MissingStateEntryProvenance
        );
    }
    for workflow_id in [&equal.id, &unsequenced.id] {
        assert_eq!(
            by_id[workflow_id.as_str()].provenance_status,
            super::store::DriverlessProgressProvenanceStatus::AmbiguousStateEntryProvenance
        );
    }
    Ok(())
}

#[tokio::test]
async fn driverless_progress_is_bounded_and_ordered_by_age_then_id() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }
    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let older_b = driverless_progress_instance(&store, "older-b", "implementing").await?;
    let older_a = driverless_progress_instance(&store, "older-a", "implementing").await?;
    let newer = driverless_progress_instance(&store, "newer", "implementing").await?;
    for instance in [&older_b, &older_a, &newer] {
        accepted_state_entry(&store, instance, "implementing").await?;
    }
    let older_at = Utc::now() - Duration::hours(2);
    sqlx::query("UPDATE workflow_instances SET updated_at = $1 WHERE id = ANY($2::text[])")
        .bind(older_at)
        .bind(vec![older_b.id.clone(), older_a.id.clone()])
        .execute(store.pool())
        .await?;

    let rows = store.list_driverless_progress_instances(2).await?;
    assert_eq!(rows.len(), 2);
    let mut expected = vec![older_a.id, older_b.id];
    expected.sort();
    assert_eq!(
        rows.iter()
            .map(|row| row.workflow_id.clone())
            .collect::<Vec<_>>(),
        expected
    );
    assert!(rows.iter().all(|row| row.age_secs >= 7_000));
    Ok(())
}

async fn driverless_progress_instance(
    store: &WorkflowRuntimeStore,
    id: &str,
    state: &str,
) -> anyhow::Result<WorkflowInstance> {
    let instance = issue_instance(state).with_id(format!("driverless-progress-{id}"));
    store.upsert_instance(&instance).await?;
    Ok(instance)
}

async fn accepted_state_entry(
    store: &WorkflowRuntimeStore,
    instance: &WorkflowInstance,
    next_state: &str,
) -> anyhow::Result<WorkflowDecisionRecord> {
    let event = store
        .append_event(&instance.id, "test", "test", json!({}))
        .await?;
    let decision_id = format!("{}-decision-{}", instance.id, event.sequence);
    persist_state_entry(
        store,
        instance,
        next_state,
        event.id,
        &decision_id,
    )
    .await
}

async fn accepted_state_entry_with_id(
    store: &WorkflowRuntimeStore,
    instance: &WorkflowInstance,
    next_state: &str,
    decision_id: &str,
) -> anyhow::Result<WorkflowDecisionRecord> {
    let event = store
        .append_event(&instance.id, "test", "test", json!({}))
        .await?;
    persist_state_entry(store, instance, next_state, event.id, decision_id).await
}

async fn persist_state_entry(
    store: &WorkflowRuntimeStore,
    instance: &WorkflowInstance,
    next_state: &str,
    event_id: String,
    decision_id: &str,
) -> anyhow::Result<WorkflowDecisionRecord> {
    let mut record = WorkflowDecisionRecord::accepted(
        WorkflowDecision::new(
            &instance.id,
            "planning",
            format!("enter-{next_state}"),
            next_state,
            "state entry fixture",
        ),
        Some(event_id),
    );
    record.id = decision_id.to_string();
    store.record_decision(&record).await?;
    Ok(record)
}

async fn insert_driver_command(
    store: &WorkflowRuntimeStore,
    workflow_id: &str,
    decision_id: Option<&str>,
    dedupe_key: &str,
    status: WorkflowCommandStatus,
) -> anyhow::Result<String> {
    let command = WorkflowCommand::enqueue_activity("implement_issue", dedupe_key);
    store
        .enqueue_command_with_status(workflow_id, decision_id, &command, status)
        .await
}

async fn runtime_history_counts(
    store: &WorkflowRuntimeStore,
) -> anyhow::Result<(i64, i64, i64, i64, i64)> {
    Ok(sqlx::query_as(
        "SELECT
            (SELECT COUNT(*) FROM workflow_instances),
            (SELECT COUNT(*) FROM workflow_events),
            (SELECT COUNT(*) FROM workflow_decisions),
            (SELECT COUNT(*) FROM workflow_commands),
            (SELECT COUNT(*) FROM runtime_jobs)",
    )
    .fetch_one(store.pool())
    .await?)
}
