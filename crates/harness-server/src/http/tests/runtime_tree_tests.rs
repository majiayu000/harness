use super::*;

#[tokio::test]
async fn health_endpoint_returns_ok_and_task_count() -> anyhow::Result<()> {
    let _home_lock = crate::test_helpers::HOME_LOCK.lock().await;
    let dir = tempfile::tempdir()?;
    let state = make_read_only_route_test_state(dir.path()).await?;
    let health = call_health(state).await?;
    assert_eq!(health.status, "ok");
    assert_eq!(health.tasks, 0);
    assert!(health.persistence.degraded_subsystems.is_empty());
    assert!(!health.persistence.runtime_state_dirty);
    assert!(health.persistence.startup.stores.is_empty());
    assert_eq!(health.runtime_logs.state, "disabled");
    Ok(())
}

#[tokio::test]
async fn health_degraded_when_subsystem_missing() -> anyhow::Result<()> {
    let _home_lock = crate::test_helpers::HOME_LOCK.lock().await;
    let dir = tempfile::tempdir()?;
    let mut state = make_read_only_route_test_state(dir.path()).await?;
    Arc::get_mut(&mut state).unwrap().degraded_subsystems = vec!["runtime_state_store"];
    let health = call_health(state).await?;
    assert_eq!(health.status, "degraded");
    assert_eq!(
        health.persistence.degraded_subsystems,
        ["runtime_state_store"]
    );
    assert!(!health.persistence.runtime_state_dirty);
    Ok(())
}

#[tokio::test]
async fn workflow_runtime_tree_endpoint_returns_nested_runtime_details() -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let state = make_test_state_with_workflow_runtime(dir.path()).await?;
    let store = state
        .core
        .workflow_runtime_store
        .as_ref()
        .expect("workflow runtime store should be configured");
    let parent = harness_workflow::runtime::WorkflowInstance::new(
        harness_workflow::runtime::PROMPT_TASK_DEFINITION_ID,
        1,
        "implementing",
        harness_workflow::runtime::WorkflowSubject::new("prompt", "owner/repo"),
    )
    .with_id("prompt-task")
    .with_data(serde_json::json!({
        "project_id": "/project-a",
        "repo": "owner/repo",
    }));
    let child = harness_workflow::runtime::WorkflowInstance::new(
        "github_issue_pr",
        1,
        "replanning",
        harness_workflow::runtime::WorkflowSubject::new("issue", "issue:123"),
    )
    .with_id("issue-123")
    .with_parent(parent.id.clone())
    .with_data(serde_json::json!({
        "project_id": "/project-a",
        "repo": "owner/repo",
        "issue_number": 123,
    }));
    store.upsert_instance(&parent).await?;
    store.upsert_instance(&child).await?;
    let event = store
        .append_event(
            &child.id,
            "PlanIssueRaised",
            "workflow-runtime-test",
            serde_json::json!({ "issue_number": 123 }),
        )
        .await?;
    let decision = harness_workflow::runtime::WorkflowDecision::new(
        child.id.clone(),
        "replanning",
        "run_replan",
        "replanning",
        "Replan requested after the budget was exhausted.",
    );
    let rejected = harness_workflow::runtime::WorkflowDecisionRecord::rejected(
        decision,
        Some(event.id),
        "replan limit exhausted",
    );
    store.record_decision(&rejected).await?;
    let command = harness_workflow::runtime::WorkflowCommand::enqueue_activity(
        "replan_issue",
        "issue-123-replan-2",
    );
    let command_id = store
        .enqueue_command(&child.id, Some(&rejected.id), &command)
        .await?;
    let not_before = chrono::Utc::now() - chrono::Duration::minutes(5);
    let runtime_job = store
        .enqueue_runtime_job_with_not_before(
            &command_id,
            harness_workflow::runtime::RuntimeKind::CodexJsonrpc,
            "codex-high",
            serde_json::json!({ "workflow_id": child.id }),
            Some(not_before),
        )
        .await?;
    let prompt_packet_digest = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    store
        .record_runtime_event(
            &runtime_job.id,
            "RuntimePromptPrepared",
            serde_json::json!({
                "prompt_packet_digest": prompt_packet_digest,
            }),
        )
        .await?;
    store
        .record_runtime_event(
            &runtime_job.id,
            "ActivityResultReady",
            serde_json::json!({ "status": "succeeded" }),
        )
        .await?;
    let lease_owner = "workflow-runtime-tree-test";
    let lease_expires_at = chrono::Utc::now() + chrono::Duration::minutes(5);
    let claimed = store
        .claim_next_runtime_job(lease_owner, lease_expires_at)
        .await?
        .expect("runtime job should be claimable");
    assert_eq!(claimed.id, runtime_job.id);
    let activity_result = harness_workflow::runtime::ActivityResult::succeeded(
        "replan_issue",
        "Runtime job completed.",
    )
    .with_artifact(harness_workflow::runtime::ActivityArtifact::new(
        "activity_result_envelope",
        serde_json::json!({
            "schema": "harness.runtime.activity_result_envelope.v1",
            "extraction_strategy": "json_fence_repair",
            "outcome": "repaired_structured_output",
            "raw_status": "completed",
            "extracted_activity": "replan_issue",
            "extraction_error": null,
            "final_result": {
                "activity": "replan_issue",
                "status": "succeeded",
                "error_kind": null,
            }
        }),
    ));
    store
        .complete_runtime_job_if_owned(
            &runtime_job.id,
            lease_owner,
            lease_expires_at,
            &activity_result,
        )
        .await?
        .expect("runtime job should complete with the current lease");

    let response = workflow_runtime_app(state)
        .oneshot(
            Request::builder()
                .uri("/api/workflows/runtime/tree?project_id=%2Fproject-a&detail=full")
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    let body = response_json(response).await?;
    assert_eq!(body["total_workflows"], 2);
    assert_eq!(body["pagination"]["limit"], 100);
    assert_eq!(body["pagination"]["offset"], 0);
    assert_eq!(body["pagination"]["returned"], 2);
    assert_eq!(body["pagination"]["detail"], "full");
    assert_eq!(body["pagination"]["summary_only"], false);
    assert_eq!(body["summary"]["total_commands"], 1);
    assert_eq!(body["summary"]["total_runtime_jobs"], 1);
    assert_eq!(body["workflows"][0]["workflow"]["id"], "prompt-task");
    let child_node = &body["workflows"][0]["children"][0];
    assert_eq!(child_node["workflow"]["id"], "issue-123");
    assert_eq!(child_node["runtime_job_count"], 1);
    assert_eq!(child_node["event_count"], 1);
    assert_eq!(child_node["decision_count"], 1);
    assert_eq!(child_node["command_count"], 1);
    assert_eq!(
        child_node["decisions"][0]["rejection_reason"],
        "replan limit exhausted"
    );
    assert_eq!(
        child_node["commands"][0]["command"]["command"]["activity"],
        "replan_issue"
    );
    assert_eq!(
        child_node["commands"][0]["runtime_jobs"][0]["runtime_profile"],
        "codex-high"
    );
    assert_eq!(
        child_node["commands"][0]["runtime_jobs"][0]["not_before"],
        serde_json::json!(not_before)
    );
    assert_eq!(
        child_node["commands"][0]["runtime_jobs"][0]["runtime_event_count"],
        2
    );
    assert_eq!(
        child_node["commands"][0]["runtime_jobs"][0]["latest_runtime_event_type"],
        "ActivityResultReady"
    );
    assert_eq!(
        child_node["commands"][0]["runtime_jobs"][0]["prompt_packet_digest"],
        prompt_packet_digest
    );
    assert_eq!(
        child_node["commands"][0]["runtime_jobs"][0]["activity_result_envelope"]["outcome"],
        "repaired_structured_output"
    );
    assert_eq!(
        child_node["commands"][0]["runtime_jobs"][0]["activity_result_envelope"]
            ["extraction_strategy"],
        "json_fence_repair"
    );
    assert_eq!(
        child_node["commands"][0]["runtime_jobs"][0]["activity_result_envelope"]["final_result"]
            ["status"],
        "succeeded"
    );
    Ok(())
}

#[tokio::test]
async fn workflow_runtime_tree_endpoint_defaults_to_compact_polling_shape() -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let state = make_test_state_with_workflow_runtime(dir.path()).await?;
    let store = state
        .core
        .workflow_runtime_store
        .as_ref()
        .expect("workflow runtime store should be configured");
    let workflow = harness_workflow::runtime::WorkflowInstance::new(
        "github_issue_pr",
        1,
        "implementing",
        harness_workflow::runtime::WorkflowSubject::new("issue", "issue:1165"),
    )
    .with_id("issue-1165")
    .with_data(serde_json::json!({
        "project_id": "/project-a",
        "repo": "owner/repo",
        "issue_number": 1165,
    }));
    store.upsert_instance(&workflow).await?;
    store
        .append_event(
            &workflow.id,
            "IssueSubmitted",
            "workflow-runtime-test",
            serde_json::json!({ "large_payload": "x".repeat(1024) }),
        )
        .await?;
    let accepted = harness_workflow::runtime::WorkflowDecisionRecord::accepted(
        harness_workflow::runtime::WorkflowDecision::new(
            workflow.id.clone(),
            "implementing",
            "wait",
            "implementing",
            "No-op decision used to prove compact counts include accepted decisions.",
        ),
        None,
    );
    store.record_decision(&accepted).await?;
    let rejected = harness_workflow::runtime::WorkflowDecisionRecord::rejected(
        harness_workflow::runtime::WorkflowDecision::new(
            workflow.id.clone(),
            "implementing",
            "run_replan",
            "replanning",
            "Rejected decision should remain visible as a compact hint.",
        ),
        None,
        "replan limit exhausted",
    );
    store.record_decision(&rejected).await?;

    for index in 0..2 {
        let command = harness_workflow::runtime::WorkflowCommand::new(
            harness_workflow::runtime::WorkflowCommandType::EnqueueActivity,
            format!("issue-1165-implement-{index}"),
            serde_json::json!({
                "activity": "implement_issue",
                "large_payload": "x".repeat(1024),
            }),
        );
        let command_id = store.enqueue_command(&workflow.id, None, &command).await?;
        let runtime_job = store
            .enqueue_runtime_job(
                &command_id,
                harness_workflow::runtime::RuntimeKind::CodexJsonrpc,
                "codex-high",
                serde_json::json!({ "large_input": "x".repeat(1024) }),
            )
            .await?;
        store
            .record_runtime_event(
                &runtime_job.id,
                "RuntimePromptPrepared",
                serde_json::json!({
                    "prompt_packet_digest": "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789",
                    "large_payload": "x".repeat(1024),
                }),
            )
            .await?;
    }
    let primitive_command = harness_workflow::runtime::WorkflowCommand::new(
        harness_workflow::runtime::WorkflowCommandType::Wait,
        "issue-1165-wait",
        serde_json::json!("wait for review"),
    );
    store
        .enqueue_command(&workflow.id, None, &primitive_command)
        .await?;

    let response = workflow_runtime_app(state)
        .oneshot(
            Request::builder()
                .uri(
                    "/api/workflows/runtime/tree?project_id=%2Fproject-a&command_limit=3&job_limit=1",
                )
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    let body = response_json(response).await?;
    assert_eq!(body["pagination"]["detail"], "compact");
    assert_eq!(body["pagination"]["command_limit"], 3);
    assert_eq!(body["summary"]["total_commands"], 3);
    assert_eq!(body["summary"]["total_runtime_jobs"], 2);

    let node = &body["workflows"][0];
    assert_eq!(node["event_count"], 1);
    assert_eq!(node["decision_count"], 2);
    assert_eq!(node["rejected_decision_count"], 1);
    assert_eq!(node["command_count"], 3);
    assert_eq!(node["runtime_job_count"], 2);
    assert_eq!(node["events"].as_array().expect("events array").len(), 0);
    assert_eq!(
        node["decisions"].as_array().expect("decisions array").len(),
        1
    );
    assert_eq!(
        node["decisions"][0]["rejection_reason"],
        "replan limit exhausted"
    );
    let commands = node["commands"].as_array().expect("commands array");
    assert_eq!(commands.len(), 3);
    let implement_command = commands
        .iter()
        .find(|command| {
            command["command"]["command"]["activity"].as_str() == Some("implement_issue")
        })
        .expect("compact response should include an implement command");
    assert!(implement_command["command"]["command"]["large_payload"].is_null());
    let primitive_command = commands
        .iter()
        .find(|command| command["command"]["dedupe_key"].as_str() == Some("issue-1165-wait"))
        .expect("compact response should include the primitive command");
    assert_eq!(
        primitive_command["command"]["command"],
        serde_json::json!("wait for review")
    );
    let jobs = implement_command["runtime_jobs"]
        .as_array()
        .expect("runtime jobs array");
    assert_eq!(jobs.len(), 1);
    assert!(jobs[0]["input"].is_null());
    assert!(jobs[0]["output"].is_null());
    assert_eq!(jobs[0]["runtime_event_count"], 1);
    assert_eq!(
        jobs[0]["latest_runtime_event_type"],
        "RuntimePromptPrepared"
    );
    assert_eq!(
        jobs[0]["prompt_packet_digest"],
        "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789"
    );
    Ok(())
}

#[tokio::test]
async fn workflow_runtime_tree_endpoint_exposes_shared_projection_status() -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let state = make_test_state_with_workflow_runtime(dir.path()).await?;
    let store = state
        .core
        .workflow_runtime_store
        .as_ref()
        .expect("workflow runtime store should be configured");

    for (id, state, issue_number, extra_data) in [
        (
            "issue-active",
            "implementing",
            1201,
            serde_json::json!({
                "submission_id": "submission-active",
                "task_id": "legacy-active",
            }),
        ),
        (
            "issue-review-wait",
            "awaiting_feedback",
            1202,
            serde_json::json!({}),
        ),
        (
            "issue-blocked",
            "blocked",
            1203,
            serde_json::json!({
                "blocked_reason": "Waiting for maintainer approval.",
                "unblock_hint": "Post the approval comment, then call unblock.",
                "last_stop": {
                    "state": "blocked",
                    "activity": "implement_issue",
                    "runtime_job_id": "job-blocked",
                    "event_id": 10,
                },
            }),
        ),
        (
            "issue-terminal",
            "failed",
            1204,
            serde_json::json!({
                "failure_reason": "Runtime transport timed out.",
                "error_kind": "timeout",
                "retry_hint": "Fix the transient condition, then call retry.",
                "last_stop": {
                    "state": "failed",
                    "activity": "implement_issue",
                    "runtime_job_id": "job-failed",
                    "event_id": 11,
                },
            }),
        ),
        (
            "issue-nonretryable",
            "failed",
            1205,
            serde_json::json!({
                "failure_reason": "Missing runtime configuration.",
                "error_kind": "configuration",
            }),
        ),
        (
            "issue-cancelled",
            "cancelled",
            1206,
            serde_json::json!({
                "failure_reason": "Operator cancelled the workflow.",
            }),
        ),
    ] {
        let mut data = serde_json::json!({
            "project_id": "/project-a",
            "repo": "owner/repo",
            "issue_number": issue_number,
        });
        data.as_object_mut()
            .expect("test data should be an object")
            .extend(
                extra_data
                    .as_object()
                    .expect("extra data should be an object")
                    .clone(),
            );
        let workflow = harness_workflow::runtime::WorkflowInstance::new(
            "github_issue_pr",
            1,
            state,
            harness_workflow::runtime::WorkflowSubject::new(
                "issue",
                format!("issue:{issue_number}"),
            ),
        )
        .with_id(id)
        .with_data(data);
        store.upsert_instance(&workflow).await?;
    }
    let blocked_runtime_job_id = set_recovery_source_job(
        store,
        "issue-blocked",
        harness_workflow::runtime::WorkflowCommand::enqueue_activity(
            "implement_issue",
            "issue-blocked-source",
        ),
    )
    .await?;
    let failed_runtime_job_id = set_recovery_source_job(
        store,
        "issue-terminal",
        harness_workflow::runtime::WorkflowCommand::enqueue_activity(
            "implement_issue",
            "issue-terminal-source",
        ),
    )
    .await?;

    let response = workflow_runtime_app(state)
        .oneshot(
            Request::builder()
                .uri("/api/workflows/runtime/tree?project_id=%2Fproject-a&job_limit=0")
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    let body = response_json(response).await?;
    let workflows = body["workflows"]
        .as_array()
        .expect("workflows should be an array");
    let node = |id: &str| {
        workflows
            .iter()
            .find(|node| node["workflow"]["id"].as_str() == Some(id))
            .unwrap_or_else(|| panic!("missing workflow node {id}"))
    };

    let active = node("issue-active");
    assert_eq!(active["projection"]["status"], "implementing");
    assert_eq!(active["projection"]["phase"], "implement");
    assert_eq!(
        active["projection"]["scheduler"]["authority_state"],
        "running"
    );
    assert_eq!(active["projection"]["active_bucket"], "running");
    assert_eq!(active["projection"]["project_id"], "/project-a");
    assert_eq!(
        active["projection"]["submission_handle"],
        "submission-active"
    );
    assert_eq!(
        active["projection"]["legacy_dedupe_task_handle"],
        "legacy-active"
    );

    let review_wait = node("issue-review-wait");
    assert_eq!(review_wait["projection"]["status"], "waiting");
    assert_eq!(review_wait["projection"]["phase"], "review");
    assert_eq!(
        review_wait["projection"]["scheduler"]["authority_state"],
        "queued"
    );
    assert_eq!(review_wait["projection"]["active_bucket"], "queued");

    let blocked = node("issue-blocked");
    assert_eq!(blocked["projection"]["status"], "waiting");
    assert_eq!(blocked["projection"]["phase"], "plan");
    assert_eq!(
        blocked["projection"]["blocked_reason"],
        "Waiting for maintainer approval."
    );
    assert_eq!(
        blocked["projection"]["unblock_hint"],
        "Post the approval comment, then call unblock."
    );
    assert_eq!(
        blocked["projection"]["last_stop"]["activity"],
        "implement_issue"
    );
    assert_eq!(
        blocked["projection"]["last_stop"]["runtime_job_id"],
        blocked_runtime_job_id
    );
    assert_eq!(blocked["projection"]["can_unblock"], true);
    assert_eq!(blocked["projection"]["can_retry"], false);

    let terminal = node("issue-terminal");
    assert_eq!(terminal["projection"]["status"], "failed");
    assert_eq!(terminal["projection"]["phase"], "terminal");
    assert_eq!(
        terminal["projection"]["scheduler"]["authority_state"],
        "failed"
    );
    assert_eq!(terminal["projection"]["failure_kind"], "task");
    assert!(terminal["projection"]["active_bucket"].is_null());
    assert_eq!(
        terminal["projection"]["failure_reason"],
        "Runtime transport timed out."
    );
    assert_eq!(terminal["projection"]["error_kind"], "timeout");
    assert_eq!(
        terminal["projection"]["retry_hint"],
        "Fix the transient condition, then call retry."
    );
    assert_eq!(
        terminal["projection"]["last_stop"]["runtime_job_id"],
        failed_runtime_job_id
    );
    assert_eq!(terminal["projection"]["can_unblock"], false);
    assert_eq!(terminal["projection"]["can_retry"], true);

    let nonretryable = node("issue-nonretryable");
    assert_eq!(nonretryable["projection"]["error_kind"], "configuration");
    assert_eq!(nonretryable["projection"]["can_unblock"], false);
    assert_eq!(nonretryable["projection"]["can_retry"], false);

    let cancelled = node("issue-cancelled");
    assert_eq!(cancelled["projection"]["status"], "cancelled");
    assert_eq!(cancelled["projection"]["can_unblock"], false);
    assert_eq!(cancelled["projection"]["can_retry"], false);

    Ok(())
}

async fn set_recovery_source_job(
    store: &harness_workflow::runtime::WorkflowRuntimeStore,
    workflow_id: &str,
    command: harness_workflow::runtime::WorkflowCommand,
) -> anyhow::Result<String> {
    let command_id = store.enqueue_command(workflow_id, None, &command).await?;
    let job = store
        .enqueue_runtime_job(
            &command_id,
            harness_workflow::runtime::RuntimeKind::CodexJsonrpc,
            "codex-test",
            command.command.clone(),
        )
        .await?;
    let runtime_job_id = job.id.clone();
    let mut workflow = store
        .get_instance(workflow_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("missing workflow {workflow_id}"))?;
    workflow.data["last_stop"]["runtime_job_id"] = serde_json::json!(runtime_job_id.clone());
    store.upsert_instance(&workflow).await?;
    Ok(runtime_job_id)
}

#[tokio::test]
async fn workflow_runtime_tree_endpoint_returns_summary_only_shape() -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let state = make_test_state_with_workflow_runtime(dir.path()).await?;
    let store = state
        .core
        .workflow_runtime_store
        .as_ref()
        .expect("workflow runtime store should be configured");
    let workflow = harness_workflow::runtime::WorkflowInstance::new(
        "github_issue_pr",
        1,
        "implementing",
        harness_workflow::runtime::WorkflowSubject::new("issue", "issue:1166"),
    )
    .with_id("issue-1166")
    .with_data(serde_json::json!({
        "project_id": "/project-a",
        "repo": "owner/repo",
        "issue_number": 1166,
    }));
    store.upsert_instance(&workflow).await?;
    let command = harness_workflow::runtime::WorkflowCommand::enqueue_activity(
        "implement_issue",
        "issue-1166-implement",
    );
    let command_id = store.enqueue_command(&workflow.id, None, &command).await?;
    store
        .enqueue_runtime_job(
            &command_id,
            harness_workflow::runtime::RuntimeKind::CodexJsonrpc,
            "codex-high",
            serde_json::json!({ "workflow_id": workflow.id }),
        )
        .await?;

    let response = workflow_runtime_app(state)
        .oneshot(
            Request::builder()
                .uri("/api/workflows/runtime/tree?project_id=%2Fproject-a&summary_only=true")
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    let body = response_json(response).await?;
    assert_eq!(body["total_workflows"], 1);
    assert_eq!(body["pagination"]["returned"], 0);
    assert_eq!(body["pagination"]["has_more"], false);
    assert_eq!(body["pagination"]["next_offset"], serde_json::Value::Null);
    assert_eq!(body["pagination"]["summary_only"], true);
    assert_eq!(body["pagination"]["detail"], "compact");
    assert_eq!(body["summary"]["workflow_statuses"]["implementing"], 1);
    assert_eq!(body["summary"]["workflow_scheduler_states"]["running"], 1);
    assert_eq!(body["summary"]["workflow_active_buckets"]["running"], 1);
    assert_eq!(body["summary"]["total_commands"], 1);
    assert_eq!(body["summary"]["total_runtime_jobs"], 1);
    assert_eq!(
        body["workflows"]
            .as_array()
            .expect("workflows should be an array")
            .len(),
        0
    );
    Ok(())
}

#[tokio::test]
async fn workflow_runtime_tree_summary_only_counts_all_project_workflows_with_tiny_limit(
) -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let state = make_test_state_with_workflow_runtime(dir.path()).await?;
    let store = state
        .core
        .workflow_runtime_store
        .as_ref()
        .expect("workflow runtime store should be configured");
    for (id, project_id, issue_number) in [
        ("issue-1201", "/project-a", 1201),
        ("issue-1202", "/project-a", 1202),
        ("issue-2201", "/project-b", 2201),
    ] {
        let workflow = harness_workflow::runtime::WorkflowInstance::new(
            "github_issue_pr",
            1,
            "implementing",
            harness_workflow::runtime::WorkflowSubject::new(
                "issue",
                format!("issue:{issue_number}"),
            ),
        )
        .with_id(id)
        .with_data(serde_json::json!({
            "project_id": project_id,
            "repo": "owner/repo",
            "issue_number": issue_number,
        }));
        store.upsert_instance(&workflow).await?;
    }

    let response = workflow_runtime_app(state)
        .oneshot(
            Request::builder()
                .uri(
                    "/api/workflows/runtime/tree?project_id=%2Fproject-a&summary_only=true&limit=1",
                )
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    let body = response_json(response).await?;
    assert_eq!(body["total_workflows"], 2);
    assert_eq!(body["pagination"]["limit"], 1);
    assert_eq!(body["pagination"]["returned"], 0);
    assert_eq!(body["pagination"]["total"], 2);
    assert_eq!(body["pagination"]["has_more"], false);
    assert_eq!(body["pagination"]["summary_only"], true);
    assert_eq!(body["summary"]["workflow_statuses"]["implementing"], 2);
    assert_eq!(body["summary"]["workflow_scheduler_states"]["running"], 2);
    assert_eq!(body["summary"]["workflow_active_buckets"]["running"], 2);
    assert_eq!(body["summary"]["total_commands"], 0);
    assert_eq!(body["summary"]["total_runtime_jobs"], 0);
    assert_eq!(
        body["workflows"]
            .as_array()
            .expect("workflows should be an array")
            .len(),
        0
    );
    Ok(())
}

#[tokio::test]
async fn workflow_runtime_tree_endpoint_summarizes_all_project_workflows_when_paginated(
) -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let state = make_test_state_with_workflow_runtime(dir.path()).await?;
    let store = state
        .core
        .workflow_runtime_store
        .as_ref()
        .expect("workflow runtime store should be configured");
    for (id, project_id, issue_number) in [
        ("issue-101", "/project-a", 101),
        ("issue-102", "/project-a", 102),
        ("issue-201", "/project-b", 201),
    ] {
        let workflow = harness_workflow::runtime::WorkflowInstance::new(
            "github_issue_pr",
            1,
            "replanning",
            harness_workflow::runtime::WorkflowSubject::new(
                "issue",
                format!("issue:{issue_number}"),
            ),
        )
        .with_id(id)
        .with_data(serde_json::json!({
            "project_id": project_id,
            "repo": "owner/repo",
            "issue_number": issue_number,
        }));
        store.upsert_instance(&workflow).await?;
        let command = harness_workflow::runtime::WorkflowCommand::enqueue_activity(
            "replan_issue",
            format!("replan-{issue_number}"),
        );
        let command_id = store.enqueue_command(&workflow.id, None, &command).await?;
        store
            .enqueue_runtime_job(
                &command_id,
                harness_workflow::runtime::RuntimeKind::CodexJsonrpc,
                "codex-high",
                serde_json::json!({ "workflow_id": workflow.id }),
            )
            .await?;
    }
    let quality_gate = harness_workflow::runtime::WorkflowInstance::new(
        harness_workflow::runtime::QUALITY_GATE_DEFINITION_ID,
        1,
        "passed",
        harness_workflow::runtime::WorkflowSubject::new("quality_gate", "issue:101"),
    )
    .with_id("quality-gate-101")
    .with_data(serde_json::json!({
        "project_id": "/project-a",
        "repo": "owner/repo",
        "issue_number": 101,
    }));
    store.upsert_instance(&quality_gate).await?;
    let non_terminal_passed_issue = harness_workflow::runtime::WorkflowInstance::new(
        harness_workflow::runtime::GITHUB_ISSUE_PR_DEFINITION_ID,
        1,
        "passed",
        harness_workflow::runtime::WorkflowSubject::new("issue", "issue:103"),
    )
    .with_id("issue-103")
    .with_data(serde_json::json!({
        "project_id": "/project-a",
        "repo": "owner/repo",
        "issue_number": 103,
    }));
    store.upsert_instance(&non_terminal_passed_issue).await?;

    let response = workflow_runtime_app(state)
        .oneshot(
            Request::builder()
                .uri("/api/workflows/runtime/tree?project_id=%2Fproject-a&limit=1")
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    let body = response_json(response).await?;
    assert_eq!(body["total_workflows"], 4);
    assert_eq!(body["pagination"]["returned"], 1);
    assert_eq!(body["pagination"]["total"], 4);
    assert_eq!(body["pagination"]["has_more"], true);
    assert_eq!(
        body["workflows"]
            .as_array()
            .expect("workflows should be an array")
            .len(),
        1
    );
    assert_eq!(body["summary"]["workflow_statuses"]["done"], 1);
    assert_eq!(body["summary"]["workflow_statuses"]["implementing"], 2);
    assert_eq!(body["summary"]["workflow_statuses"]["waiting"], 1);
    assert_eq!(body["summary"]["workflow_scheduler_states"]["done"], 1);
    assert_eq!(body["summary"]["workflow_scheduler_states"]["queued"], 1);
    assert_eq!(body["summary"]["workflow_scheduler_states"]["running"], 2);
    assert_eq!(body["summary"]["workflow_active_buckets"]["queued"], 1);
    assert_eq!(body["summary"]["workflow_active_buckets"]["running"], 2);
    assert_eq!(body["summary"]["total_commands"], 2);
    assert_eq!(body["summary"]["total_runtime_jobs"], 2);
    assert_eq!(body["summary"]["command_statuses"]["pending"], 2);
    assert_eq!(body["summary"]["runtime_job_statuses"]["pending"], 2);
    assert_eq!(body["summary"]["jobs_without_activity_envelope"], 2);
    Ok(())
}

#[tokio::test]
async fn workflow_runtime_tree_endpoint_splits_running_lease_states() -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let state = make_test_state_with_workflow_runtime(dir.path()).await?;
    let store = state
        .core
        .workflow_runtime_store
        .as_ref()
        .expect("workflow runtime store should be configured");
    let workflow = harness_workflow::runtime::WorkflowInstance::new(
        "github_issue_pr",
        1,
        "implementing",
        harness_workflow::runtime::WorkflowSubject::new("issue", "issue:1170"),
    )
    .with_id("issue-1170")
    .with_data(serde_json::json!({
        "project_id": "/project-a",
        "repo": "owner/repo",
        "issue_number": 1170,
    }));
    store.upsert_instance(&workflow).await?;

    for activity in ["implement_issue", "inspect_pr_feedback"] {
        let command =
            harness_workflow::runtime::WorkflowCommand::enqueue_activity(activity, activity);
        let command_id = store.enqueue_command(&workflow.id, None, &command).await?;
        store
            .enqueue_runtime_job(
                &command_id,
                harness_workflow::runtime::RuntimeKind::CodexJsonrpc,
                "codex-high",
                serde_json::json!({
                    "workflow_id": workflow.id,
                    "activity": activity,
                }),
            )
            .await?;
    }

    let active = store
        .claim_next_runtime_job(
            "active-worker",
            chrono::Utc::now() + chrono::Duration::minutes(5),
        )
        .await?
        .expect("active runtime job should be claimed");
    store
        .record_runtime_event(
            &active.id,
            "RuntimeTurnStarted",
            serde_json::json!({ "owner": "active-worker" }),
        )
        .await?;
    let expired = store
        .claim_next_runtime_job(
            "expired-worker",
            chrono::Utc::now() - chrono::Duration::minutes(5),
        )
        .await?
        .expect("expired runtime job should be claimed");

    let response = workflow_runtime_app(state)
        .oneshot(
            Request::builder()
                .uri("/api/workflows/runtime/tree?project_id=%2Fproject-a")
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    let body = response_json(response).await?;
    assert_eq!(body["summary"]["runtime_job_statuses"]["running"], 2);
    assert_eq!(
        body["summary"]["running_job_lease_statuses"]["active_leased"],
        1
    );
    assert_eq!(
        body["summary"]["running_job_lease_statuses"]["expired_lease"],
        1
    );

    let commands = body["workflows"][0]["commands"]
        .as_array()
        .expect("commands should be an array");
    let active_job = commands
        .iter()
        .flat_map(|command| command["runtime_jobs"].as_array().into_iter().flatten())
        .find(|job| job["id"] == active.id)
        .expect("active job should be exposed");
    assert_eq!(active_job["lease_state"], "active_leased");
    assert_eq!(active_job["in_flight_model_turn"], true);
    assert!(active_job["last_runtime_observation_at"].is_string());

    let expired_job = commands
        .iter()
        .flat_map(|command| command["runtime_jobs"].as_array().into_iter().flatten())
        .find(|job| job["id"] == expired.id)
        .expect("expired job should be exposed");
    assert_eq!(expired_job["lease_state"], "expired_lease");
    assert_eq!(expired_job["in_flight_model_turn"], false);

    Ok(())
}

#[tokio::test]
async fn workflow_runtime_tree_endpoint_limits_runtime_jobs_per_command() -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let state = make_test_state_with_workflow_runtime(dir.path()).await?;
    let store = state
        .core
        .workflow_runtime_store
        .as_ref()
        .expect("workflow runtime store should be configured");
    let workflow = harness_workflow::runtime::WorkflowInstance::new(
        "github_issue_pr",
        1,
        "replanning",
        harness_workflow::runtime::WorkflowSubject::new("issue", "issue:456"),
    )
    .with_id("issue-456")
    .with_data(serde_json::json!({
        "project_id": "/project-a",
        "repo": "owner/repo",
        "issue_number": 456,
    }));
    store.upsert_instance(&workflow).await?;
    let command =
        harness_workflow::runtime::WorkflowCommand::enqueue_activity("replan_issue", "replan-456");
    let command_id = store.enqueue_command(&workflow.id, None, &command).await?;
    for index in 0..7 {
        store
            .enqueue_runtime_job(
                &command_id,
                harness_workflow::runtime::RuntimeKind::CodexJsonrpc,
                "codex-high",
                serde_json::json!({ "index": index }),
            )
            .await?;
    }

    let response = workflow_runtime_app(state)
        .oneshot(
            Request::builder()
                .uri("/api/workflows/runtime/tree?project_id=%2Fproject-a&job_limit=2")
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    let body = response_json(response).await?;
    let node = &body["workflows"][0];
    let command_node = &node["commands"][0];
    assert_eq!(body["summary"]["total_commands"], 1);
    assert_eq!(body["summary"]["total_runtime_jobs"], 7);
    assert_eq!(body["pagination"]["job_limit"], 2);
    assert_eq!(node["runtime_job_count"], 7);
    assert_eq!(command_node["runtime_job_count"], 7);
    assert_eq!(
        command_node["runtime_jobs"]
            .as_array()
            .expect("runtime_jobs should be an array")
            .len(),
        2
    );
    Ok(())
}
