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
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let received = Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let received_server = Arc::clone(&received);
    tokio::spawn(async move {
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
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{response_body}",
            response_body.len()
        );
        let _ = socket.write_all(response.as_bytes()).await;
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
    let tick = super::background::run_runtime_pr_feedback_sweep_tick(&state, 10).await?;

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
