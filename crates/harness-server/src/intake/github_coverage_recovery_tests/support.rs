use super::*;
use serde_json::{json, Value};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub(super) const REPO: &str = "owner/repo";

pub(super) async fn open_runtime_store(
) -> anyhow::Result<Option<(tempfile::TempDir, WorkflowRuntimeStore)>> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(None);
    }
    let dir = crate::test_helpers::tempdir_in_home("harness-test-coverage-recovery-")?;
    let store = WorkflowRuntimeStore::open_with_database_url(
        &dir.path().join("runtime"),
        Some(&crate::test_helpers::test_database_url()?),
    )
    .await?;
    Ok(Some((dir, store)))
}

pub(super) async fn recover_with_urls(
    store: &WorkflowRuntimeStore,
    project_root: &Path,
    project_id: &str,
    issue_number: u64,
    _rest_url: &str,
    graphql_url: &str,
) -> anyhow::Result<GitHubIssueCoverage> {
    recover_github_pr_coverage_with_client(
        store,
        project_root,
        project_id,
        REPO,
        issue_number,
        None,
        &reqwest::Client::new(),
        graphql_url,
    )
    .await
}

pub(super) async fn assert_recovered_binding(
    store: &WorkflowRuntimeStore,
    project_id: &str,
    issue_number: u64,
    pr_number: u64,
    state: &str,
) -> anyhow::Result<()> {
    let workflow = store
        .get_instance(&workflow_id(project_id, Some(REPO), issue_number))
        .await?
        .expect("recovered workflow");
    assert_eq!(workflow.state, state);
    assert_eq!(workflow.data["issue_number"], issue_number);
    assert_eq!(workflow.data["pr_number"], pr_number);
    assert_eq!(workflow.data["coverage_recovered_from_github"], true);
    assert!(workflow.data["last_remote_fact_hash"].as_str().is_some());
    Ok(())
}

pub(super) async fn assert_no_agent_work(
    store: &WorkflowRuntimeStore,
    project_id: &str,
    issue_number: u64,
) -> anyhow::Result<()> {
    let id = workflow_id(project_id, Some(REPO), issue_number);
    assert!(store
        .commands_for(&id)
        .await?
        .iter()
        .all(|command| command.status == WorkflowCommandStatus::Cancelled));
    assert!(store.pending_commands(500).await?.is_empty());
    Ok(())
}

pub(super) async fn assert_quality_gate_queued(
    store: &WorkflowRuntimeStore,
    project_id: &str,
    issue_number: u64,
    pr_number: u64,
) -> anyhow::Result<()> {
    let id = workflow_id(project_id, Some(REPO), issue_number);
    let commands = store.commands_for(&id).await?;
    assert_eq!(commands.len(), 1);
    assert_eq!(
        commands[0].command.command_type,
        WorkflowCommandType::StartChildWorkflow
    );
    assert_eq!(
        commands[0].command.command["definition_id"],
        QUALITY_GATE_DEFINITION_ID
    );
    assert_eq!(commands[0].command.command["pr_number"], pr_number);
    Ok(())
}

pub(super) fn rest_candidates(prs: &[(u64, u64)]) -> Value {
    Value::Array(
        prs.iter()
            .map(|(pr, issue)| {
                json!({
                    "number": pr,
                    "html_url": format!("https://github.com/{REPO}/pull/{pr}"),
                    "title": format!("Fix issue {issue}"),
                    "body": format!("Closes #{issue}"),
                    "head": {"ref": format!("fix-{issue}-{pr}")},
                })
            })
            .collect(),
    )
}

pub(super) fn pr_snapshot(
    pr_number: u64,
    issue_number: u64,
    state: &str,
    checks: &str,
    approved: bool,
) -> Value {
    json!({
        "number": pr_number,
        "state": state,
        "merged": state == "MERGED",
        "url": format!("https://github.com/{REPO}/pull/{pr_number}"),
        "title": format!("Fix issue {issue_number}"),
        "baseRefName": "main",
        "headRefName": format!("fix-{issue_number}-{pr_number}"),
        "headRefOid": format!("head-{pr_number}"),
        "mergeCommit": {"oid": if state == "MERGED" { Some("merge-sha") } else { None }},
        "isDraft": false,
        "mergeStateStatus": "CLEAN",
        "reviewDecision": if approved { "APPROVED" } else { "REVIEW_REQUIRED" },
        "statusCheckRollup": {"state": checks},
        "reviewThreads": {"pageInfo": {"hasNextPage": false, "endCursor": null}, "nodes": []},
        "files": {"pageInfo": {"hasNextPage": false, "endCursor": null}, "nodes": []},
        "closingIssuesReferences": {
            "pageInfo": {"hasNextPage": false, "endCursor": null},
            "nodes": [{"number": issue_number, "url": format!("https://github.com/{REPO}/issues/{issue_number}")}],
        },
    })
}

pub(super) fn graphql_response(pr: Value) -> Value {
    json!({"data": {"repository": {"pullRequest": pr}}})
}

pub(super) fn issue_links_response(issue_state: &str, prs: &[u64]) -> Value {
    json!({"data": {"repository": {"issue": {
        "state": issue_state,
        "closedByPullRequestsReferences": {
            "pageInfo": {"hasNextPage": false, "endCursor": null},
            "nodes": prs.iter().map(|pr| json!({
                "number": pr,
                "url": format!("https://github.com/{REPO}/pull/{pr}"),
                "headRefName": format!("linked-{pr}"),
                "repository": {"nameWithOwner": REPO},
            })).collect::<Vec<_>>(),
        },
    }}}})
}

pub(super) async fn spawn_json_server(status: &'static str, bodies: Vec<Value>) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind GitHub mock");
    let address = listener.local_addr().expect("GitHub mock address");
    let bodies = Arc::new(
        bodies
            .into_iter()
            .map(|body| body.to_string())
            .collect::<Vec<_>>(),
    );
    let request_count = Arc::new(AtomicUsize::new(0));
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let bodies = Arc::clone(&bodies);
            let index = request_count.fetch_add(1, Ordering::SeqCst);
            tokio::spawn(async move {
                let mut request = [0_u8; 16_384];
                let _ = socket.read(&mut request).await;
                let body = &bodies[index.min(bodies.len().saturating_sub(1))];
                let response = format!(
                    "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len(),
                );
                let _ = socket.write_all(response.as_bytes()).await;
            });
        }
    });
    format!("http://{address}")
}
