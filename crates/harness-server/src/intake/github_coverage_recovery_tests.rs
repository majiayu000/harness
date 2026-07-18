use super::*;
use serde_json::{json, Value};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const REPO: &str = "owner/repo";

#[tokio::test]
async fn empty_store_recovers_ready_pr_and_stays_idempotent_after_restart() -> anyhow::Result<()> {
    let Some((dir, store)) = open_runtime_store().await? else {
        return Ok(());
    };
    let project_root = dir.path().join("project");
    std::fs::create_dir(&project_root)?;
    let project_id = project_root.to_string_lossy().into_owned();
    let issue_number = 1707;
    let pr_number = 1710;
    let rest_url = spawn_json_server(
        "200 OK",
        vec![rest_candidates(&[(pr_number, issue_number)])],
    )
    .await;
    let graphql_url = spawn_json_server(
        "200 OK",
        vec![graphql_response(pr_snapshot(
            pr_number,
            issue_number,
            "OPEN",
            "SUCCESS",
            true,
        ))],
    )
    .await;

    let coverage = recover_with_urls(
        &store,
        &project_root,
        &project_id,
        issue_number,
        &rest_url,
        &graphql_url,
    )
    .await?;
    assert_eq!(
        coverage,
        GitHubIssueCoverage::Covered {
            source: "github_closing_pr",
            state: "ready_to_merge".to_string(),
        }
    );
    assert_recovered_binding(
        &store,
        &project_id,
        issue_number,
        pr_number,
        "ready_to_merge",
    )
    .await?;
    assert_no_agent_work(&store, &project_id, issue_number).await?;

    let repeated = check_github_issue_coverage(
        None,
        Some(&store),
        &project_root,
        &project_id,
        REPO,
        issue_number,
        None,
    )
    .await?;
    assert!(matches!(repeated, GitHubIssueCoverage::Covered { .. }));
    assert_no_agent_work(&store, &project_id, issue_number).await?;

    drop(store);
    let reopened = WorkflowRuntimeStore::open_with_database_url(
        &dir.path().join("runtime"),
        Some(&crate::test_helpers::test_database_url()?),
    )
    .await?;
    let after_restart = check_github_issue_coverage(
        None,
        Some(&reopened),
        &project_root,
        &project_id,
        REPO,
        issue_number,
        None,
    )
    .await?;
    assert!(matches!(after_restart, GitHubIssueCoverage::Covered { .. }));
    assert_recovered_binding(
        &reopened,
        &project_id,
        issue_number,
        pr_number,
        "ready_to_merge",
    )
    .await?;
    assert_no_agent_work(&reopened, &project_id, issue_number).await?;
    Ok(())
}

#[tokio::test]
async fn recovery_maps_open_feedback_and_merged_pr_facts() -> anyhow::Result<()> {
    let Some((dir, store)) = open_runtime_store().await? else {
        return Ok(());
    };

    for (index, (pr_state, checks, approved, expected_state)) in [
        ("OPEN", "PENDING", false, "pr_open"),
        ("OPEN", "FAILURE", false, "awaiting_feedback"),
        ("MERGED", "SUCCESS", true, "done"),
    ]
    .into_iter()
    .enumerate()
    {
        let issue_number = 1720 + index as u64;
        let pr_number = 1730 + index as u64;
        let project_root = dir.path().join(format!("project-{index}"));
        std::fs::create_dir(&project_root)?;
        let project_id = project_root.to_string_lossy().into_owned();
        let rest_url = spawn_json_server(
            "200 OK",
            vec![rest_candidates(&[(pr_number, issue_number)])],
        )
        .await;
        let graphql_url = spawn_json_server(
            "200 OK",
            vec![graphql_response(pr_snapshot(
                pr_number,
                issue_number,
                pr_state,
                checks,
                approved,
            ))],
        )
        .await;

        let coverage = recover_with_urls(
            &store,
            &project_root,
            &project_id,
            issue_number,
            &rest_url,
            &graphql_url,
        )
        .await?;
        assert_eq!(
            coverage,
            GitHubIssueCoverage::Covered {
                source: "github_closing_pr",
                state: expected_state.to_string(),
            }
        );
        assert_recovered_binding(&store, &project_id, issue_number, pr_number, expected_state)
            .await?;
        let fact = store
            .get_remote_fact_snapshot("github", REPO, "pull_request", pr_number as i64)
            .await?
            .expect("server-owned PR fact");
        assert_eq!(fact.facts["pr_number"], pr_number);
        if expected_state == "done" {
            let workflow = store
                .get_instance(&workflow_id(&project_id, Some(REPO), issue_number))
                .await?
                .expect("merged workflow");
            assert_eq!(
                workflow.data["terminal_evidence"]["reason"],
                "closing_pr_merged"
            );
            assert_eq!(
                workflow.data["terminal_evidence"]["merge_commit_sha"],
                "merge-sha"
            );
        }
        assert_no_agent_work(&store, &project_id, issue_number).await?;
    }
    Ok(())
}

#[tokio::test]
async fn closed_pr_is_uncovered_but_a_later_valid_pr_recovers_coverage() -> anyhow::Result<()> {
    let Some((dir, store)) = open_runtime_store().await? else {
        return Ok(());
    };
    let project_root = dir.path().join("closed-only");
    std::fs::create_dir(&project_root)?;
    let project_id = project_root.to_string_lossy().into_owned();
    let issue_number = 1740;
    let rest_url =
        spawn_json_server("200 OK", vec![rest_candidates(&[(1741, issue_number)])]).await;
    let graphql_url = spawn_json_server(
        "200 OK",
        vec![graphql_response(pr_snapshot(
            1741,
            issue_number,
            "CLOSED",
            "SUCCESS",
            true,
        ))],
    )
    .await;

    let coverage = recover_with_urls(
        &store,
        &project_root,
        &project_id,
        issue_number,
        &rest_url,
        &graphql_url,
    )
    .await?;
    assert_eq!(coverage, GitHubIssueCoverage::Uncovered);
    assert!(store
        .get_instance(&workflow_id(&project_id, Some(REPO), issue_number))
        .await?
        .is_none());
    assert!(store
        .get_remote_fact_snapshot("github", REPO, "pull_request", 1741)
        .await?
        .is_some());

    let second_root = dir.path().join("closed-then-open");
    std::fs::create_dir(&second_root)?;
    let second_project = second_root.to_string_lossy().into_owned();
    let rest_url = spawn_json_server(
        "200 OK",
        vec![rest_candidates(&[
            (1741, issue_number),
            (1742, issue_number),
        ])],
    )
    .await;
    let graphql_url = spawn_json_server(
        "200 OK",
        vec![
            graphql_response(pr_snapshot(1741, issue_number, "CLOSED", "SUCCESS", true)),
            graphql_response(pr_snapshot(1742, issue_number, "OPEN", "PENDING", false)),
        ],
    )
    .await;
    let coverage = recover_with_urls(
        &store,
        &second_root,
        &second_project,
        issue_number,
        &rest_url,
        &graphql_url,
    )
    .await?;
    assert!(matches!(coverage, GitHubIssueCoverage::Covered { .. }));
    assert_recovered_binding(&store, &second_project, issue_number, 1742, "pr_open").await?;
    assert_no_agent_work(&store, &second_project, issue_number).await?;
    Ok(())
}

#[tokio::test]
async fn github_lookup_failure_is_visible_and_leaves_no_dispatchable_work() -> anyhow::Result<()> {
    let Some((dir, store)) = open_runtime_store().await? else {
        return Ok(());
    };
    let project_root = dir.path().join("lookup-failure");
    std::fs::create_dir(&project_root)?;
    let project_id = project_root.to_string_lossy().into_owned();
    let rest_url =
        spawn_json_server("503 Service Unavailable", vec![json!({"error": "down"})]).await;
    let graphql_url = spawn_json_server("200 OK", vec![json!({})]).await;

    let error = recover_with_urls(
        &store,
        &project_root,
        &project_id,
        1750,
        &rest_url,
        &graphql_url,
    )
    .await
    .expect_err("lookup failure must fail closed");
    assert!(error.to_string().contains("503 Service Unavailable"));
    assert!(store
        .get_instance(&workflow_id(&project_id, Some(REPO), 1750))
        .await?
        .is_none());
    assert_no_agent_work(&store, &project_id, 1750).await?;
    Ok(())
}

async fn open_runtime_store() -> anyhow::Result<Option<(tempfile::TempDir, WorkflowRuntimeStore)>> {
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

async fn recover_with_urls(
    store: &WorkflowRuntimeStore,
    project_root: &Path,
    project_id: &str,
    issue_number: u64,
    rest_url: &str,
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
        rest_url,
        graphql_url,
    )
    .await
}

async fn assert_recovered_binding(
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

async fn assert_no_agent_work(
    store: &WorkflowRuntimeStore,
    project_id: &str,
    issue_number: u64,
) -> anyhow::Result<()> {
    let id = workflow_id(project_id, Some(REPO), issue_number);
    assert!(store.commands_for(&id).await?.is_empty());
    assert!(store.pending_commands(500).await?.is_empty());
    Ok(())
}

fn rest_candidates(prs: &[(u64, u64)]) -> Value {
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

fn pr_snapshot(
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
        "reviewThreads": {
            "pageInfo": {"hasNextPage": false, "endCursor": null},
            "nodes": [],
        },
        "files": {
            "pageInfo": {"hasNextPage": false, "endCursor": null},
            "nodes": [],
        },
        "closingIssuesReferences": {
            "nodes": [{
                "number": issue_number,
                "url": format!("https://github.com/{REPO}/issues/{issue_number}"),
            }],
        },
    })
}

fn graphql_response(pr: Value) -> Value {
    json!({"data": {"repository": {"pullRequest": pr}}})
}

async fn spawn_json_server(status: &'static str, bodies: Vec<Value>) -> String {
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
