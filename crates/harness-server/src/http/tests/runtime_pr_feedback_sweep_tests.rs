use super::*;

fn graphql_pr(head_oid: &str, updated_at: &str) -> serde_json::Value {
    serde_json::json!({
        "number": 77,
        "state": "OPEN",
        "merged": false,
        "url": "https://github.com/owner/repo/pull/77",
        "title": "Runtime PR feedback",
        "updatedAt": updated_at,
        "baseRefName": "main",
        "headRefName": "feature",
        "headRefOid": head_oid,
        "mergeCommit": null,
        "isDraft": false,
        "mergeStateStatus": "CLEAN",
        "reviewDecision": "CHANGES_REQUESTED",
        "statusCheckRollup": {"state": "SUCCESS"},
        "reviewThreads": {
            "pageInfo": {"hasNextPage": false, "endCursor": null},
            "nodes": [
                {
                    "id": "thread-1",
                    "path": "src/lib.rs",
                    "line": 1,
                    "isResolved": false,
                    "isOutdated": false,
                    "comments": {
                        "nodes": [
                            {
                                "author": {"login": "reviewer"},
                                "body": "needs work",
                                "publishedAt": updated_at
                            }
                        ]
                    }
                }
            ]
        },
        "files": {
            "pageInfo": {"hasNextPage": false, "endCursor": null},
            "nodes": []
        },
        "closingIssuesReferences": {
            "pageInfo": {"hasNextPage": false, "endCursor": null},
            "nodes": []
        }
    })
}

async fn spawn_graphql_stub(
    response_body: String,
) -> anyhow::Result<(String, Arc<tokio::sync::Mutex<Vec<String>>>)> {
    spawn_graphql_stub_responses(vec![(200, response_body)]).await
}

async fn spawn_graphql_stub_responses(
    responses: Vec<(u16, String)>,
) -> anyhow::Result<(String, Arc<tokio::sync::Mutex<Vec<String>>>)> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let received = Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let received_server = Arc::clone(&received);
    tokio::spawn(async move {
        for (status, response_body) in responses {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let mut buf = [0_u8; 16_384];
            let Ok(read) = socket.read(&mut buf).await else {
                return;
            };
            received_server
                .lock()
                .await
                .push(String::from_utf8_lossy(&buf[..read]).into_owned());
            let reason = if status == 200 { "OK" } else { "ERROR" };
            let response = format!(
                "HTTP/1.1 {status} {reason}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{response_body}",
                response_body.len()
            );
            let _ = socket.write_all(response.as_bytes()).await;
        }
    });
    Ok((format!("http://{addr}"), received))
}

#[tokio::test]
async fn runtime_pr_feedback_sweep_refreshes_remote_fact_before_child_suppression(
) -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    use crate::workspace::test_support::{async_env_lock, ScopedEnvVar};

    let _env_guard = async_env_lock().lock().await;
    let fresh_updated_at = "2026-07-31T00:00:00Z";
    let response_body = serde_json::json!({
        "data": {
            "repository": {
                "pullRequest": graphql_pr("fresh-head", fresh_updated_at)
            }
        }
    })
    .to_string();
    let (graphql_url, received) = spawn_graphql_stub(response_body).await?;
    let _graphql_guard = ScopedEnvVar::set("HARNESS_GITHUB_GRAPHQL_URL", &graphql_url);

    let dir = tempfile::tempdir()?;
    let project_root = dir.path().join("project-feedback-refresh");
    std::fs::create_dir(&project_root)?;
    std::fs::write(
        project_root.join("WORKFLOW.md"),
        "---\npr_feedback:\n  enabled: true\nruntime_dispatch:\n  enabled: true\nruntime_worker:\n  enabled: true\n---\n",
    )?;
    let state = make_test_state_with_workflow_runtime(dir.path()).await?;
    let store = state
        .core
        .workflow_runtime_store
        .as_ref()
        .expect("workflow runtime store should be configured");
    let workflow = harness_workflow::runtime::WorkflowInstance::new(
        "github_issue_pr",
        1,
        "awaiting_feedback",
        harness_workflow::runtime::WorkflowSubject::new("issue", "issue:226"),
    )
    .with_id("issue-226-feedback-refresh")
    .with_data(serde_json::json!({
        "project_id": project_root,
        "repo": "owner/repo",
        "issue_number": 226,
        "pr_number": 77,
        "pr_url": "https://github.com/owner/repo/pull/77",
        "task_id": "runtime-task-226",
    }));
    store.upsert_instance(&workflow).await?;
    let child = harness_workflow::runtime::WorkflowInstance::new(
        harness_workflow::runtime::PR_FEEDBACK_DEFINITION_ID,
        1,
        "no_actionable_feedback",
        harness_workflow::runtime::WorkflowSubject::new("pr", "pr:77"),
    )
    .with_id("issue-226-feedback-refresh-child")
    .with_parent(workflow.id.clone())
    .with_data(serde_json::json!({
        "remote_fact_hash": "sha256:stale",
        "remote_fact_activity_at": "2026-07-30T00:00:00Z",
    }));
    store.upsert_instance(&child).await?;
    let tick = super::background::run_runtime_pr_feedback_sweep_tick(&state, 2).await?;

    assert_eq!(tick.requested, 1);
    assert_eq!(tick.active_command_exists, 0);
    assert_eq!(tick.skipped, 0);
    assert_eq!(tick.rejected, 0);
    assert_eq!(received.lock().await.len(), 1);
    let persisted = store
        .get_remote_fact_snapshot("github", "owner/repo", "pull_request", 77)
        .await?
        .expect("fresh PR fact should be persisted before suppression");
    assert_ne!(persisted.fact_hash, "sha256:stale");
    assert_eq!(persisted.facts["head_oid"], "fresh-head");
    let commands = store.commands_for(&workflow.id).await?;
    assert_eq!(commands.len(), 1);
    assert_eq!(
        commands[0].command.command["remote_fact_hash"],
        persisted.fact_hash
    );
    assert_eq!(
        commands[0].command.command["remote_fact_activity_at"],
        fresh_updated_at
    );
    Ok(())
}

#[tokio::test]
async fn runtime_pr_feedback_sweep_continues_after_refresh_failure() -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    use crate::workspace::test_support::{async_env_lock, ScopedEnvVar};

    let _env_guard = async_env_lock().lock().await;
    let fresh_updated_at = "2026-07-31T00:00:00Z";
    let success_body = serde_json::json!({
        "data": {
            "repository": {
                "pullRequest": graphql_pr("fresh-head", fresh_updated_at)
            }
        }
    })
    .to_string();
    let (graphql_url, received) =
        spawn_graphql_stub_responses(vec![(500, "unavailable".to_string()), (200, success_body)])
            .await?;
    let _graphql_guard = ScopedEnvVar::set("HARNESS_GITHUB_GRAPHQL_URL", &graphql_url);

    let dir = tempfile::tempdir()?;
    let project_root = dir.path().join("project-feedback-refresh-continues");
    std::fs::create_dir(&project_root)?;
    std::fs::write(
        project_root.join("WORKFLOW.md"),
        "---\npr_feedback:\n  enabled: true\nruntime_dispatch:\n  enabled: true\nruntime_worker:\n  enabled: true\n---\n",
    )?;
    let state = make_test_state_with_workflow_runtime(dir.path()).await?;
    let store = state
        .core
        .workflow_runtime_store
        .as_ref()
        .expect("workflow runtime store should be configured");
    let later_workflow = harness_workflow::runtime::WorkflowInstance::new(
        "github_issue_pr",
        1,
        "awaiting_feedback",
        harness_workflow::runtime::WorkflowSubject::new("issue", "issue:227"),
    )
    .with_id("issue-227-feedback-refresh-success")
    .with_data(serde_json::json!({
        "project_id": project_root.to_string_lossy(),
        "repo": "owner/repo",
        "issue_number": 227,
        "pr_number": 77,
        "pr_url": "https://github.com/owner/repo/pull/77",
        "task_id": "runtime-task-227",
    }));
    store.upsert_instance(&later_workflow).await?;
    let failing_workflow = harness_workflow::runtime::WorkflowInstance::new(
        "github_issue_pr",
        1,
        "awaiting_feedback",
        harness_workflow::runtime::WorkflowSubject::new("issue", "issue:226"),
    )
    .with_id("issue-226-feedback-refresh-fails")
    .with_data(serde_json::json!({
        "project_id": project_root.to_string_lossy(),
        "repo": "owner/repo",
        "issue_number": 226,
        "pr_number": 77,
        "pr_url": "https://github.com/owner/repo/pull/77",
        "task_id": "runtime-task-226",
    }));
    store.upsert_instance(&failing_workflow).await?;
    let later_updated_at =
        chrono::DateTime::parse_from_rfc3339("2099-01-01T00:00:01Z")?.with_timezone(&chrono::Utc);
    let failing_updated_at =
        chrono::DateTime::parse_from_rfc3339("2099-01-01T00:00:02Z")?.with_timezone(&chrono::Utc);
    sqlx::query("UPDATE workflow_instances SET updated_at = $2 WHERE id = $1")
        .bind(&later_workflow.id)
        .bind(later_updated_at)
        .execute(store.pool())
        .await?;
    sqlx::query("UPDATE workflow_instances SET updated_at = $2 WHERE id = $1")
        .bind(&failing_workflow.id)
        .bind(failing_updated_at)
        .execute(store.pool())
        .await?;

    let tick = super::background::run_runtime_pr_feedback_sweep_tick(&state, 2).await?;

    assert_eq!(tick.requested, 1);
    assert_eq!(tick.rejected, 1);
    assert_eq!(tick.active_command_exists, 0);
    assert_eq!(tick.skipped, 0);
    assert_eq!(received.lock().await.len(), 2);
    assert_eq!(store.commands_for(&later_workflow.id).await?.len(), 1);
    assert!(store.commands_for(&failing_workflow.id).await?.is_empty());
    Ok(())
}

#[tokio::test]
async fn runtime_pr_feedback_sweep_skips_active_driver_without_spending_work_limit(
) -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }

    use crate::workspace::test_support::{async_env_lock, ScopedEnvVar};

    let _env_guard = async_env_lock().lock().await;
    let response_body = serde_json::json!({
        "data": {
            "repository": {
                "pullRequest": graphql_pr("unused-head", "2026-07-31T00:00:00Z")
            }
        }
    })
    .to_string();
    let (graphql_url, received) = spawn_graphql_stub(response_body).await?;
    let _graphql_guard = ScopedEnvVar::set("HARNESS_GITHUB_GRAPHQL_URL", &graphql_url);

    let dir = tempfile::tempdir()?;
    let project_root = dir.path().join("project-active-feedback-driver");
    std::fs::create_dir(&project_root)?;
    std::fs::write(
        project_root.join("WORKFLOW.md"),
        "---\npr_feedback:\n  enabled: true\nruntime_dispatch:\n  enabled: true\nruntime_worker:\n  enabled: true\n---\n",
    )?;
    let state = make_test_state_with_workflow_runtime(dir.path()).await?;
    let store = state
        .core
        .workflow_runtime_store
        .as_ref()
        .expect("workflow runtime store should be configured");

    let older_pr_open = harness_workflow::runtime::WorkflowInstance::new(
        "github_issue_pr",
        1,
        "pr_open",
        harness_workflow::runtime::WorkflowSubject::new("issue", "issue:228"),
    )
    .with_id("issue-228-pr-open")
    .with_data(serde_json::json!({
        "project_id": project_root.to_string_lossy(),
        "repo": "owner/repo",
        "issue_number": 228,
        "pr_number": 78,
        "pr_url": "https://github.com/owner/repo/pull/78",
        "task_id": "runtime-task-228",
    }));
    store.upsert_instance(&older_pr_open).await?;

    let newer_awaiting_feedback = harness_workflow::runtime::WorkflowInstance::new(
        "github_issue_pr",
        1,
        "awaiting_feedback",
        harness_workflow::runtime::WorkflowSubject::new("issue", "issue:229"),
    )
    .with_id("issue-229-active-feedback-driver")
    .with_data(serde_json::json!({
        "project_id": project_root.to_string_lossy(),
        "repo": "owner/repo",
        "issue_number": 229,
        "pr_number": 77,
        "pr_url": "https://github.com/owner/repo/pull/77",
        "task_id": "runtime-task-229",
    }));
    store.upsert_instance(&newer_awaiting_feedback).await?;
    let child = harness_workflow::runtime::WorkflowInstance::new(
        harness_workflow::runtime::PR_FEEDBACK_DEFINITION_ID,
        1,
        "inspecting",
        harness_workflow::runtime::WorkflowSubject::new("pr", "pr:77"),
    )
    .with_id("issue-229-active-feedback-driver-child")
    .with_parent(newer_awaiting_feedback.id.clone());
    store.upsert_instance(&child).await?;
    let inspect = harness_workflow::runtime::WorkflowCommand::enqueue_activity(
        harness_workflow::runtime::PR_FEEDBACK_INSPECT_ACTIVITY,
        "issue-229-active-inspection",
    );
    store.enqueue_command(&child.id, None, &inspect).await?;

    let older_updated_at =
        chrono::DateTime::parse_from_rfc3339("2099-01-01T00:00:01Z")?.with_timezone(&chrono::Utc);
    let newer_updated_at =
        chrono::DateTime::parse_from_rfc3339("2099-01-01T00:00:02Z")?.with_timezone(&chrono::Utc);
    sqlx::query("UPDATE workflow_instances SET updated_at = $2 WHERE id = $1")
        .bind(&older_pr_open.id)
        .bind(older_updated_at)
        .execute(store.pool())
        .await?;
    sqlx::query("UPDATE workflow_instances SET updated_at = $2 WHERE id = $1")
        .bind(&newer_awaiting_feedback.id)
        .bind(newer_updated_at)
        .execute(store.pool())
        .await?;

    let tick = super::background::run_runtime_pr_feedback_sweep_tick(&state, 1).await?;

    assert_eq!(tick.requested, 1);
    assert_eq!(tick.active_command_exists, 1);
    assert_eq!(tick.skipped, 0);
    assert_eq!(tick.rejected, 0);
    assert!(
        received.lock().await.is_empty(),
        "an active feedback driver must suppress the remote refresh"
    );
    assert_eq!(store.commands_for(&older_pr_open.id).await?.len(), 1);
    Ok(())
}
