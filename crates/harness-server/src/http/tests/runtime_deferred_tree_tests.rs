use super::*;

#[tokio::test]
async fn runtime_tree_reports_deferred_command() -> anyhow::Result<()> {
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
    let project_id = "/project-deferred-tree";
    let workflow = harness_workflow::runtime::WorkflowInstance::new(
        "github_issue_pr",
        1,
        "implementing",
        harness_workflow::runtime::WorkflowSubject::new("issue", "issue:1601"),
    )
    .with_id("issue-1601-deferred-tree")
    .with_data(serde_json::json!({
        "project_id": project_id,
        "repo": "owner/repo",
        "issue_number": 1601,
    }));
    store.upsert_instance(&workflow).await?;
    let command = harness_workflow::runtime::WorkflowCommand::enqueue_activity(
        "implement_issue",
        "issue-1601-deferred-tree-command",
    );
    let command_id = store.enqueue_command(&workflow.id, None, &command).await?;
    let claim = store
        .claim_pending_commands(
            "tree-dispatcher",
            chrono::Utc::now() + chrono::Duration::minutes(1),
            10,
        )
        .await?
        .into_iter()
        .find(|record| record.id == command_id)
        .expect("command should be claimed");
    let outcome = store
        .defer_claimed_command_if_owned(
            &command_id,
            "tree-dispatcher",
            claim.dispatch_claim_generation,
            harness_workflow::runtime::DispatchBarrierInput::new(
                harness_workflow::runtime::DispatchBarrierReasonCode::IsolationTierUnavailable,
                "container runtime is unavailable",
                project_id,
            )
            .with_isolation("container", "non_collaborator"),
            chrono::Utc::now(),
            harness_workflow::runtime::DispatchBackoffPolicy::from_seconds(5, 20)?,
        )
        .await?;
    let harness_workflow::runtime::DeferClaimedCommandOutcome::Deferred(barrier) = outcome else {
        panic!("command should be deferred")
    };

    let response = workflow_runtime_app(state.clone())
        .oneshot(
            Request::builder()
                .uri("/api/workflows/runtime/tree?project_id=%2Fproject-deferred-tree&detail=full")
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    let body = response_json(response).await?;
    assert_eq!(body["summary"]["command_statuses"]["deferred"], 1);
    let command_node = &body["workflows"][0]["commands"][0];
    assert_eq!(command_node["id"], command_id);
    assert_eq!(command_node["status"], "deferred");
    assert_eq!(command_node["dispatch_attempt_count"], 1);
    assert_eq!(command_node["dispatch_claim_generation"], 1);
    assert!(command_node["dispatch_not_before"].is_string());
    assert_eq!(
        command_node["dispatch_barrier"]["reason_code"],
        "isolation_tier_unavailable"
    );
    assert_eq!(
        command_node["dispatch_barrier"]["reason"],
        "container runtime is unavailable"
    );
    assert_eq!(
        command_node["dispatch_barrier"]["required_tier"],
        "container"
    );
    assert_eq!(
        command_node["dispatch_barrier"]["trust_class"],
        "non_collaborator"
    );

    sqlx::query("UPDATE workflow_commands SET dispatch_barrier = NULL WHERE id = $1")
        .bind(&command_id)
        .execute(store.pool())
        .await?;
    let missing_response = workflow_runtime_app(state.clone())
        .oneshot(
            Request::builder()
                .uri("/api/workflows/runtime/tree?project_id=%2Fproject-deferred-tree&detail=full")
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(missing_response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let missing_body = response_json(missing_response).await?;
    assert!(missing_body["error"]
        .as_str()
        .expect("missing evidence error should be visible")
        .contains("missing dispatch barrier evidence"));
    sqlx::query("UPDATE workflow_commands SET dispatch_barrier = $2::jsonb WHERE id = $1")
        .bind(&command_id)
        .bind(serde_json::to_string(&barrier)?)
        .execute(store.pool())
        .await?;

    sqlx::query(
        "UPDATE workflow_commands
         SET dispatch_barrier = '{\"reason_code\":\"runtime_policy_disabled\"}'::jsonb
         WHERE id = $1",
    )
    .bind(&command_id)
    .execute(store.pool())
    .await?;
    let invalid_response = workflow_runtime_app(state)
        .oneshot(
            Request::builder()
                .uri("/api/workflows/runtime/tree?project_id=%2Fproject-deferred-tree&detail=full")
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(invalid_response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let invalid_body = response_json(invalid_response).await?;
    assert!(invalid_body["error"]
        .as_str()
        .expect("invalid evidence error should be visible")
        .contains("dispatch barrier evidence is invalid"));
    Ok(())
}
