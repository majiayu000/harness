#[test]
fn operator_actions_link_evidence_to_current_legacy_task_id() {
    let mut ready = workflow(
        "ready_to_merge",
        json!({
            "repo": "owner/repo",
            "pr_number": 7,
            "pr_url": "https://github.com/owner/repo/pull/7",
            "submission_id": "stable-submission",
            "task_id": "current-task",
        }),
    )
    .with_id("ready-workflow".to_string());
    ready.updated_at = Utc::now();

    let actions = operator_actions(
        &WorkflowDefinitionRegistry::with_builtins(),
        &[ready],
        Utc::now(),
        &std::collections::HashMap::new(),
    );

    assert_eq!(actions.len(), 1);
    assert_eq!(actions[0].task_id.as_deref(), Some("current-task"));
    assert_eq!(
        actions[0].evidence_url.as_deref(),
        Some("/api/workflows/runtime/submissions/current-task")
    );
}

#[test]
fn operator_monitor_actions_expose_structured_stop_metadata_and_eligibility() {
    let blocked = workflow(
        "blocked",
        json!({
            "repo": "owner/repo",
            "issue_number": 1567,
            "blocked_reason": "Waiting for maintainer approval.",
            "unblock_hint": "Post the approval comment, then call unblock.",
            "last_stop": {
                "state": "blocked",
                "activity": "implement_issue",
                "runtime_job_id": "job-blocked"
            },
        }),
    )
    .with_id("blocked-workflow".to_string());
    let retryable_failed = workflow(
        "failed",
        json!({
            "repo": "owner/repo",
            "issue_number": 1568,
            "failure_reason": "Runtime transport timed out.",
            "error_kind": "timeout",
            "retry_hint": "Fix the transient condition, then call retry.",
            "last_stop": {
                "state": "failed",
                "activity": "implement_issue",
                "runtime_job_id": "job-failed"
            },
        }),
    )
    .with_id("retryable-failed-workflow".to_string());
    let configuration_failed = workflow(
        "failed",
        json!({
            "repo": "owner/repo",
            "issue_number": 1569,
            "failure_reason": "Missing runtime configuration.",
            "error_kind": "configuration",
            "retry_hint": "Fix the non-retryable failure before retrying.",
        }),
    )
    .with_id("configuration-failed-workflow".to_string());
    let cancelled = workflow(
        "cancelled",
        json!({
            "repo": "owner/repo",
            "issue_number": 1570,
            "failure_reason": "Operator cancelled the workflow.",
        }),
    )
    .with_id("cancelled-workflow".to_string());

    let mut stopped_eligibility = std::collections::HashMap::new();
    stopped_eligibility.insert(
        "blocked-workflow".to_string(),
        RuntimeStoppedActionEligibility {
            can_unblock: true,
            can_retry: false,
        },
    );
    stopped_eligibility.insert(
        "retryable-failed-workflow".to_string(),
        RuntimeStoppedActionEligibility {
            can_unblock: false,
            can_retry: true,
        },
    );
    let actions = operator_actions(
        &WorkflowDefinitionRegistry::with_builtins(),
        &[blocked, retryable_failed, configuration_failed, cancelled],
        Utc::now(),
        &stopped_eligibility,
    );
    let row = |workflow_id: &str| {
        actions
            .iter()
            .find(|action| action.workflow_id == workflow_id)
            .map(|action| serde_json::to_value(action).expect("action should serialize"))
            .unwrap_or_else(|| panic!("missing operator action for {workflow_id}"))
    };

    let blocked = row("blocked-workflow");
    assert_eq!(blocked["kind"], "blocked");
    assert_eq!(
        blocked["blocked_reason"],
        "Waiting for maintainer approval."
    );
    assert_eq!(
        blocked["unblock_hint"],
        "Post the approval comment, then call unblock."
    );
    assert_eq!(blocked["last_stop"]["activity"], "implement_issue");
    assert_eq!(blocked["can_unblock"], true);
    assert_eq!(blocked["can_retry"], false);

    let retryable_failed = row("retryable-failed-workflow");
    assert_eq!(retryable_failed["kind"], "failed");
    assert_eq!(
        retryable_failed["failure_reason"],
        "Runtime transport timed out."
    );
    assert_eq!(retryable_failed["error_kind"], "timeout");
    assert_eq!(retryable_failed["next_action"], "Retry failed workflow");
    assert_eq!(
        retryable_failed["retry_hint"],
        "Fix the transient condition, then call retry."
    );
    assert_eq!(
        retryable_failed["last_stop"]["runtime_job_id"],
        "job-failed"
    );
    assert_eq!(retryable_failed["can_unblock"], false);
    assert_eq!(retryable_failed["can_retry"], true);

    let configuration_failed = row("configuration-failed-workflow");
    assert_eq!(configuration_failed["error_kind"], "configuration");
    assert_eq!(
        configuration_failed["next_action"],
        "Inspect failed workflow"
    );
    assert_eq!(configuration_failed["can_unblock"], false);
    assert_eq!(configuration_failed["can_retry"], false);
    assert!(actions
        .iter()
        .all(|action| action.workflow_id != "cancelled-workflow"));
}

#[test]
fn operator_monitor_stuck_workflows_expose_structured_stop_metadata() {
    let blocked = workflow(
        "blocked",
        json!({
            "repo": "owner/repo",
            "issue_number": 1567,
            "blocked_reason": "Waiting for maintainer approval.",
            "unblock_hint": "Post the approval comment, then call unblock.",
            "last_stop": {
                "state": "blocked",
                "activity": "implement_issue",
                "runtime_job_id": "job-blocked",
            },
        }),
    )
    .with_id("stuck-blocked-workflow".to_string());

    let mut stopped_eligibility = std::collections::HashMap::new();
    stopped_eligibility.insert(
        "stuck-blocked-workflow".to_string(),
        RuntimeStoppedActionEligibility {
            can_unblock: true,
            can_retry: false,
        },
    );
    let stuck = stuck_workflows_from_instances(
        &WorkflowDefinitionRegistry::with_builtins(),
        &[blocked],
        Utc::now(),
        &stopped_eligibility,
    );
    let row = serde_json::to_value(&stuck[0]).expect("stuck workflow should serialize");

    assert_eq!(row["workflow_id"], "stuck-blocked-workflow");
    assert_eq!(row["blocked_reason"], "Waiting for maintainer approval.");
    assert_eq!(
        row["unblock_hint"],
        "Post the approval comment, then call unblock."
    );
    assert_eq!(row["last_stop"]["runtime_job_id"], "job-blocked");
    assert_eq!(row["can_unblock"], true);
    assert_eq!(row["can_retry"], false);
}

#[test]
fn operator_monitor_stuck_workflows_expose_auto_recovery_fields() {
    // GH-1584 exposure: persisted classification and attempt state surface
    // as optional fields; legacy rows omit them entirely (B-014).
    let classified = workflow(
        "blocked",
        json!({
            "repo": "owner/repo",
            "blocked_reason": "GitHub API rate limited",
            "stop_reason_code": "rate_limited",
            "reason_class": "transient",
            "auto_recovery": {
                "episode_event_id": "episode-1",
                "attempts": 2,
                "next_attempt_at": "2026-07-14T10:00:00Z",
                "exhausted": false,
            },
            "last_stop": {
                "state": "blocked",
                "activity": "implement_issue",
                "event_id": "episode-1",
            },
        }),
    )
    .with_id("stuck-auto-recovery-workflow".to_string());
    let legacy = workflow(
        "blocked",
        json!({ "repo": "owner/repo", "blocked_reason": "legacy free text" }),
    )
    .with_id("stuck-legacy-workflow".to_string());

    let stuck = stuck_workflows_from_instances(
        &WorkflowDefinitionRegistry::with_builtins(),
        &[classified, legacy],
        Utc::now(),
        &std::collections::HashMap::new(),
    );
    let by_id = |id: &str| {
        stuck
            .iter()
            .find(|row| row.workflow_id == id)
            .map(|row| serde_json::to_value(row).expect("stuck workflow should serialize"))
            .expect("row present")
    };

    let classified_row = by_id("stuck-auto-recovery-workflow");
    assert_eq!(classified_row["stop_reason_code"], "rate_limited");
    assert_eq!(classified_row["reason_class"], "transient");
    assert_eq!(classified_row["auto_recovery_attempts"], 2);
    assert_eq!(classified_row["next_recheck_at"], "2026-07-14T10:00:00Z");
    assert_eq!(classified_row["auto_recovery_exhausted"], false);

    let legacy_row = by_id("stuck-legacy-workflow");
    for field in [
        "stop_reason_code",
        "reason_class",
        "auto_recovery_attempts",
        "next_recheck_at",
        "auto_recovery_exhausted",
    ] {
        assert!(
            legacy_row.get(field).is_none(),
            "legacy rows must omit {field} instead of fabricating it"
        );
    }
}

#[tokio::test]
async fn stopped_action_eligibility_matches_recovery_contract_rejections() -> anyhow::Result<()> {
    if !test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let _lock = test_helpers::HOME_LOCK.lock().await;
    let dir = test_helpers::tempdir_in_home("harness-test-stopped-action-eligibility-")?;
    let store = open_operator_workflow_store(dir.path()).await?;

    let legacy_blocked = store_workflow(
        &store,
        workflow("blocked", json!({ "blocked_reason": "legacy blocked row" }))
            .with_id("legacy-blocked".to_string()),
    )
    .await?;
    let legacy_null_last_stop = store_workflow(
        &store,
        workflow(
            "failed",
            json!({
                "failure_reason": "Legacy workflow failed before structured metadata shipped.",
                "last_stop": null,
            }),
        )
        .with_id("legacy-null-last-stop".to_string()),
    )
    .await?;
    let valid_blocked = store_stopped_workflow_with_source(
        &store,
        "valid-blocked",
        "blocked",
        json!({
            "blocked_reason": "Waiting for maintainer approval.",
            "last_stop": {
                "state": "blocked",
                "activity": "implement_issue",
            },
        }),
        WorkflowCommand::enqueue_activity("implement_issue", "valid-blocked-source"),
    )
    .await?;
    let valid_unknown = store_stopped_workflow_with_source(
        &store,
        "valid-unknown",
        "failed",
        json!({
            "error_kind": "unknown",
            "last_stop": {
                "state": "failed",
                "activity": "implement_issue",
                "error_kind": "unknown",
            },
        }),
        WorkflowCommand::enqueue_activity("implement_issue", "valid-unknown-source"),
    )
    .await?;
    let non_github = store_quality_gate_workflow(
        &store,
        "non-github",
        "blocked",
        json!({
            "last_stop": {
                "state": "blocked",
                "activity": "implement_issue",
            },
        }),
        WorkflowCommand::enqueue_activity("implement_issue", "non-github-source"),
    )
    .await?;
    let empty_last_stop = store_workflow(
        &store,
        workflow("blocked", json!({ "last_stop": {} })).with_id("empty-last-stop".to_string()),
    )
    .await?;
    let partial_last_stop = store_workflow(
        &store,
        workflow(
            "failed",
            json!({
                "error_kind": "timeout",
                "last_stop": {
                    "state": "failed",
                    "activity": "implement_issue",
                },
            }),
        )
        .with_id("partial-last-stop".to_string()),
    )
    .await?;
    let non_object_last_stop = store_workflow(
        &store,
        workflow("blocked", json!({ "last_stop": 42 })).with_id("non-object-last-stop".to_string()),
    )
    .await?;
    let invalid_error_kind = store_stopped_workflow_with_source(
        &store,
        "invalid-error-kind",
        "failed",
        json!({
            "error_kind": "not_a_kind",
            "last_stop": {
                "state": "failed",
                "activity": "implement_issue",
            },
        }),
        WorkflowCommand::enqueue_activity("implement_issue", "invalid-error-kind-source"),
    )
    .await?;
    let unsupported_activity = store_stopped_workflow_with_source(
        &store,
        "unsupported-activity",
        "failed",
        json!({
            "error_kind": "timeout",
            "last_stop": {
                "state": "failed",
                "activity": "quality_gate",
            },
        }),
        WorkflowCommand::enqueue_activity("quality_gate", "unsupported-activity-source"),
    )
    .await?;
    let missing_runtime_job_id = store_workflow(
        &store,
        workflow(
            "blocked",
            json!({
                "last_stop": {
                    "state": "blocked",
                    "activity": "implement_issue",
                },
            }),
        )
        .with_id("missing-runtime-job-id".to_string()),
    )
    .await?;
    let missing_source_command = store_workflow(
        &store,
        workflow(
            "failed",
            json!({
                "error_kind": "timeout",
                "last_stop": {
                    "state": "failed",
                    "activity": "implement_issue",
                    "runtime_job_id": "missing-runtime-job",
                },
            }),
        )
        .with_id("missing-source-command".to_string()),
    )
    .await?;
    let command_target_mismatch = store_stopped_workflow_with_source(
        &store,
        "command-target-mismatch",
        "failed",
        json!({
            "error_kind": "timeout",
            "last_stop": {
                "state": "failed",
                "activity": "implement_issue",
            },
        }),
        WorkflowCommand::enqueue_activity("replan_issue", "command-target-mismatch-source"),
    )
    .await?;

    let workflows = vec![
        legacy_blocked,
        legacy_null_last_stop,
        valid_blocked,
        valid_unknown,
        non_github,
        empty_last_stop,
        partial_last_stop,
        non_object_last_stop,
        invalid_error_kind,
        unsupported_activity,
        missing_runtime_job_id,
        missing_source_command,
        command_target_mismatch,
    ];
    let eligibility = stopped_action_eligibility_for_workflows(Some(&store), &workflows).await?;
    let flags = |workflow_id: &str| eligibility.get(workflow_id).copied().unwrap_or_default();

    assert_eq!(
        flags("legacy-blocked"),
        RuntimeStoppedActionEligibility {
            can_unblock: true,
            can_retry: false,
        }
    );
    assert_eq!(
        flags("legacy-null-last-stop"),
        RuntimeStoppedActionEligibility {
            can_unblock: false,
            can_retry: true,
        }
    );
    assert_eq!(
        flags("valid-blocked"),
        RuntimeStoppedActionEligibility {
            can_unblock: true,
            can_retry: false,
        }
    );
    assert_eq!(
        flags("valid-unknown"),
        RuntimeStoppedActionEligibility {
            can_unblock: false,
            can_retry: true,
        }
    );
    for workflow_id in [
        "non-github",
        "empty-last-stop",
        "partial-last-stop",
        "non-object-last-stop",
        "invalid-error-kind",
        "unsupported-activity",
        "missing-runtime-job-id",
        "missing-source-command",
        "command-target-mismatch",
    ] {
        assert_eq!(
            flags(workflow_id),
            RuntimeStoppedActionEligibility::default(),
            "{workflow_id}"
        );
    }
    Ok(())
}

async fn store_workflow(
    store: &WorkflowRuntimeStore,
    workflow: WorkflowInstance,
) -> anyhow::Result<WorkflowInstance> {
    crate::test_helpers::force_upsert_runtime_lifecycle_state_for_test(store, &workflow).await?;
    Ok(workflow)
}

async fn store_stopped_workflow_with_source(
    store: &WorkflowRuntimeStore,
    workflow_id: &str,
    state: &str,
    data: Value,
    command: WorkflowCommand,
) -> anyhow::Result<WorkflowInstance> {
    let workflow = workflow("implementing", data).with_id(workflow_id.to_string());
    crate::test_helpers::force_upsert_runtime_lifecycle_state_for_test(store, &workflow).await?;
    let mut workflow = attach_recovery_source_job(store, workflow, command).await?;
    workflow.state = state.to_string();
    crate::test_helpers::force_upsert_runtime_lifecycle_state_for_test(store, &workflow).await?;
    Ok(workflow)
}

async fn store_quality_gate_workflow(
    store: &WorkflowRuntimeStore,
    workflow_id: &str,
    state: &str,
    data: Value,
    command: WorkflowCommand,
) -> anyhow::Result<WorkflowInstance> {
    let workflow = WorkflowInstance::new(
        QUALITY_GATE_DEFINITION_ID,
        1,
        "checking",
        WorkflowSubject::new("quality_gate", workflow_id),
    )
    .with_id(workflow_id.to_string())
    .with_server_data(data);
    crate::test_helpers::force_upsert_runtime_lifecycle_state_for_test(store, &workflow).await?;
    let mut workflow = attach_recovery_source_job(store, workflow, command).await?;
    workflow.state = state.to_string();
    crate::test_helpers::force_upsert_runtime_lifecycle_state_for_test(store, &workflow).await?;
    Ok(workflow)
}

async fn attach_recovery_source_job(
    store: &WorkflowRuntimeStore,
    mut workflow: WorkflowInstance,
    command: WorkflowCommand,
) -> anyhow::Result<WorkflowInstance> {
    let command_id = store.enqueue_command(&workflow.id, None, &command).await?;
    let job = store
        .enqueue_runtime_job(
            &command_id,
            RuntimeKind::CodexJsonrpc,
            "codex-test",
            command.command.clone(),
        )
        .await?;
    let mut last_stop = workflow.data["last_stop"].clone();
    last_stop["runtime_job_id"] = json!(job.id);
    workflow.set_data_field(
        "last_stop",
        last_stop,
        harness_workflow::runtime::DataProvenance::Server,
    )?;
    crate::test_helpers::force_upsert_runtime_lifecycle_state_for_test(store, &workflow).await?;
    Ok(workflow)
}

#[tokio::test]
async fn operator_action_age_uses_store_updated_at() -> anyhow::Result<()> {
    let _lock = test_helpers::HOME_LOCK.lock().await;
    let dir = test_helpers::tempdir_in_home("harness-test-operator-monitor-action-age-")?;
    let workflow_runtime_store = open_operator_workflow_store(dir.path()).await?;
    let mut ready = workflow(
        "ready_to_merge",
        json!({
            "source": "github",
            "pr_number": 7,
            "pr_url": "https://github.com/owner/repo/pull/7",
        }),
    )
    .with_id("ready-store-age".to_string());
    ready.updated_at = Utc::now() - chrono::Duration::days(2);
    crate::test_helpers::force_upsert_runtime_lifecycle_state_for_test(
        &workflow_runtime_store,
        &ready,
    )
    .await?;
    sqlx::query("UPDATE workflow_instances SET updated_at = NOW() WHERE id = $1")
        .bind("ready-store-age")
        .execute(workflow_runtime_store.pool())
        .await?;

    let workflows = list_runtime_workflows_from_store(&workflow_runtime_store).await?;
    let actions = operator_actions(
        &WorkflowDefinitionRegistry::with_builtins(),
        &workflows,
        Utc::now(),
        &std::collections::HashMap::new(),
    );

    let action = actions
        .iter()
        .find(|action| action.workflow_id == "ready-store-age")
        .expect("ready action");
    assert!(
        action.age_secs < 60,
        "action age should use the fresh store timestamp, got {}",
        action.age_secs
    );
    Ok(())
}
