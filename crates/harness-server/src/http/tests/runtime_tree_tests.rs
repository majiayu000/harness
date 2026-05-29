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
    Arc::get_mut(&mut state).unwrap().degraded_subsystems = vec!["q_value_store"];
    let health = call_health(state).await?;
    assert_eq!(health.status, "degraded");
    assert_eq!(health.persistence.degraded_subsystems, ["q_value_store"]);
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
        harness_workflow::runtime::REPO_BACKLOG_DEFINITION_ID,
        1,
        "dispatching",
        harness_workflow::runtime::WorkflowSubject::new("repo", "owner/repo"),
    )
    .with_id("repo-backlog")
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
                .uri("/api/workflows/runtime/tree?project_id=%2Fproject-a")
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    let body = response_json(response).await?;
    assert_eq!(body["total_workflows"], 2);
    assert_eq!(body["pagination"]["limit"], 100);
    assert_eq!(body["pagination"]["offset"], 0);
    assert_eq!(body["pagination"]["returned"], 2);
    assert_eq!(body["summary"]["total_commands"], 1);
    assert_eq!(body["summary"]["total_runtime_jobs"], 1);
    assert_eq!(body["workflows"][0]["workflow"]["id"], "repo-backlog");
    let child_node = &body["workflows"][0]["children"][0];
    assert_eq!(child_node["workflow"]["id"], "issue-123");
    assert_eq!(child_node["runtime_job_count"], 1);
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

    let response = workflow_runtime_app(state)
        .oneshot(
            Request::builder()
                .uri("/api/workflows/runtime/tree?project_id=%2Fproject-a&limit=1")
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    let body = response_json(response).await?;
    assert_eq!(body["total_workflows"], 2);
    assert_eq!(body["pagination"]["returned"], 1);
    assert_eq!(body["pagination"]["total"], 2);
    assert_eq!(body["pagination"]["has_more"], true);
    assert_eq!(
        body["workflows"]
            .as_array()
            .expect("workflows should be an array")
            .len(),
        1
    );
    assert_eq!(body["summary"]["total_commands"], 2);
    assert_eq!(body["summary"]["total_runtime_jobs"], 2);
    assert_eq!(body["summary"]["command_statuses"]["pending"], 2);
    assert_eq!(body["summary"]["runtime_job_statuses"]["pending"], 2);
    assert_eq!(body["summary"]["jobs_without_activity_envelope"], 2);
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
