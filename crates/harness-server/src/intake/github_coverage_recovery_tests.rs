use super::*;
use serde_json::json;

#[path = "github_coverage_recovery_tests/support.rs"]
mod support;
use support::*;

#[path = "github_coverage_recovery_tests/atomic.rs"]
mod atomic;

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
        vec![
            issue_links_response("OPEN", &[pr_number]),
            graphql_response(pr_snapshot(
                pr_number,
                issue_number,
                "OPEN",
                "SUCCESS",
                true,
            )),
        ],
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
            state: "quality_gate_pending".to_string(),
        }
    );
    assert_recovered_binding(
        &store,
        &project_id,
        issue_number,
        pr_number,
        "quality_gate_pending",
    )
    .await?;
    assert_quality_gate_queued(&store, &project_id, issue_number, pr_number).await?;

    let repeated = check_github_issue_coverage(
        None,
        Some(&store),
        &project_root,
        &project_id,
        REPO,
        issue_number,
        IsolationTrustClass::Trusted,
        None,
    )
    .await?;
    assert!(matches!(repeated, GitHubIssueCoverage::Covered { .. }));
    assert_quality_gate_queued(&store, &project_id, issue_number, pr_number).await?;

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
        IsolationTrustClass::Trusted,
        None,
    )
    .await?;
    assert!(matches!(after_restart, GitHubIssueCoverage::Covered { .. }));
    assert_recovered_binding(
        &reopened,
        &project_id,
        issue_number,
        pr_number,
        "quality_gate_pending",
    )
    .await?;
    assert_quality_gate_queued(&reopened, &project_id, issue_number, pr_number).await?;
    Ok(())
}

#[tokio::test]
async fn recovery_maps_open_pr_facts() -> anyhow::Result<()> {
    let Some((dir, store)) = open_runtime_store().await? else {
        return Ok(());
    };

    for (index, (pr_state, checks, approved, expected_state)) in [
        ("OPEN", "PENDING", false, "pr_open"),
        ("OPEN", "FAILURE", false, "awaiting_feedback"),
    ]
    .into_iter()
    .enumerate()
    {
        let issue_number = 1720 + index as u64;
        let pr_number = 1730 + index as u64;
        let project_root = dir.path().join(format!("project-{index}"));
        std::fs::create_dir(&project_root)?;
        let project_id = project_root.to_string_lossy().into_owned();
        let existing = WorkflowInstance::new(
            GITHUB_ISSUE_PR_DEFINITION_ID,
            1,
            "planning",
            WorkflowSubject::new("issue", format!("issue:{issue_number}")),
        )
        .with_id(workflow_id(&project_id, Some(REPO), issue_number))
        .with_data(json!({
            "submission_id": format!("existing-handle-{issue_number}"),
            "task_id": format!("github-issue:{REPO}:issue:{issue_number}"),
            "task_ids": [format!("existing-handle-{issue_number}")],
        }));
        store.upsert_instance(&existing).await?;
        store
            .enqueue_command(
                &existing.id,
                None,
                &WorkflowCommand::enqueue_activity("plan_issue", format!("stale-{issue_number}")),
            )
            .await?;
        let rest_url = spawn_json_server(
            "200 OK",
            vec![rest_candidates(&[(pr_number, issue_number)])],
        )
        .await;
        let graphql_url = spawn_json_server(
            "200 OK",
            vec![
                issue_links_response("OPEN", &[pr_number]),
                graphql_response(pr_snapshot(
                    pr_number,
                    issue_number,
                    pr_state,
                    checks,
                    approved,
                )),
            ],
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
        let recovered = store
            .get_instance_by_submission_id(&format!("existing-handle-{issue_number}"))
            .await?
            .ok_or_else(|| anyhow::anyhow!("recovered workflow missing"))?;
        assert_eq!(
            recovered.data["submission_id"],
            format!("existing-handle-{issue_number}")
        );
        let fact = store
            .get_remote_fact_snapshot("github", REPO, "pull_request", pr_number as i64)
            .await?
            .expect("server-owned PR fact");
        assert_eq!(fact.facts["pr_number"], pr_number);
        assert_no_agent_work(&store, &project_id, issue_number).await?;
    }
    Ok(())
}

#[tokio::test]
async fn merged_pr_recovers_terminal_coverage_even_when_issue_is_open() -> anyhow::Result<()> {
    let Some((dir, store)) = open_runtime_store().await? else {
        return Ok(());
    };
    let project_root = dir.path().join("merged-open-issue");
    std::fs::create_dir(&project_root)?;
    let project_id = project_root.to_string_lossy().into_owned();
    let issue_number = 1729;
    let pr_number = 1739;
    let rest_url = spawn_json_server(
        "200 OK",
        vec![rest_candidates(&[(pr_number, issue_number)])],
    )
    .await;
    let graphql_url = spawn_json_server(
        "200 OK",
        vec![
            issue_links_response("OPEN", &[pr_number]),
            graphql_response(pr_snapshot(
                pr_number,
                issue_number,
                "MERGED",
                "SUCCESS",
                true,
            )),
        ],
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
            state: "done".to_string(),
        }
    );
    assert_recovered_binding(&store, &project_id, issue_number, pr_number, "done").await?;
    assert_no_agent_work(&store, &project_id, issue_number).await?;
    assert_eq!(
        check_github_issue_coverage(
            None,
            Some(&store),
            &project_root,
            &project_id,
            REPO,
            issue_number,
            IsolationTrustClass::Trusted,
            None,
        )
        .await?,
        GitHubIssueCoverage::Covered {
            source: "workflow_runtime",
            state: "done".to_string(),
        }
    );
    Ok(())
}

#[tokio::test]
async fn merged_pr_and_closed_issue_recover_terminal_coverage_without_agent_work(
) -> anyhow::Result<()> {
    let Some((dir, store)) = open_runtime_store().await? else {
        return Ok(());
    };
    let project_root = dir.path().join("merged-closed-issue");
    std::fs::create_dir(&project_root)?;
    let project_id = project_root.to_string_lossy().into_owned();
    let issue_number = 1730;
    let pr_number = 1740;
    let rest_url = spawn_json_server("200 OK", vec![rest_candidates(&[])]).await;
    let graphql_url = spawn_json_server(
        "200 OK",
        vec![
            issue_links_response("CLOSED", &[pr_number]),
            graphql_response(pr_snapshot(
                pr_number,
                issue_number,
                "MERGED",
                "SUCCESS",
                true,
            )),
        ],
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
            state: "done".to_string(),
        }
    );
    assert_recovered_binding(&store, &project_id, issue_number, pr_number, "done").await?;
    let recovered = store
        .get_instance(&workflow_id(&project_id, Some(REPO), issue_number))
        .await?
        .ok_or_else(|| anyhow::anyhow!("terminal recovered workflow is missing"))?;
    assert_eq!(
        recovered.data["terminal_evidence"]["reason"],
        "closing_pr_merged"
    );
    assert_eq!(
        recovered.data["terminal_evidence"]["merge_commit_sha"],
        "merge-sha"
    );
    assert_eq!(
        recovered.data["terminal_evidence"]["fact_hash"],
        recovered.data["last_remote_fact_hash"]
    );
    assert_no_agent_work(&store, &project_id, issue_number).await?;
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
        vec![
            issue_links_response("OPEN", &[1741]),
            graphql_response(pr_snapshot(1741, issue_number, "CLOSED", "SUCCESS", true)),
        ],
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
            issue_links_response("OPEN", &[1742]),
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
    let graphql_url =
        spawn_json_server("503 Service Unavailable", vec![json!({"error": "down"})]).await;

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

#[tokio::test]
async fn authoritative_issue_link_survives_incomplete_pr_closing_issue_snapshot(
) -> anyhow::Result<()> {
    let Some((dir, store)) = open_runtime_store().await? else {
        return Ok(());
    };
    let project_root = dir.path().join("paginated-closing-issues");
    std::fs::create_dir(&project_root)?;
    let project_id = project_root.to_string_lossy().into_owned();
    let issue_number = 1_777;
    let pr_number = 1_778;
    let rest_url = spawn_json_server(
        "200 OK",
        vec![rest_candidates(&[(pr_number, issue_number)])],
    )
    .await;
    let mut snapshot = pr_snapshot(pr_number, issue_number, "OPEN", "PENDING", false);
    snapshot["closingIssuesReferences"] = json!({
        "pageInfo": {"hasNextPage": true, "endCursor": "cursor-20"},
        "nodes": (1_u64..=20)
            .map(|number| json!({
                "number": number,
                "url": format!("https://github.com/{REPO}/issues/{number}"),
            }))
            .collect::<Vec<_>>(),
    });
    let graphql_url = spawn_json_server(
        "200 OK",
        vec![
            issue_links_response("OPEN", &[pr_number]),
            graphql_response(snapshot),
        ],
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
            state: "pr_open".to_string(),
        }
    );
    assert_recovered_binding(&store, &project_id, issue_number, pr_number, "pr_open").await?;
    Ok(())
}

#[tokio::test]
async fn issue_linked_pr_without_closing_keyword_recovers_coverage() -> anyhow::Result<()> {
    let Some((dir, store)) = open_runtime_store().await? else {
        return Ok(());
    };
    let project_root = dir.path().join("issue-linked-pr");
    std::fs::create_dir(&project_root)?;
    let project_id = project_root.to_string_lossy().into_owned();
    let issue_number = 1_780;
    let pr_number = 1_781;
    let rest_url = spawn_json_server("200 OK", vec![rest_candidates(&[])]).await;
    let mut snapshot = pr_snapshot(pr_number, issue_number, "OPEN", "PENDING", false);
    snapshot["closingIssuesReferences"]["nodes"] = json!([]);
    let graphql_url = spawn_json_server(
        "200 OK",
        vec![
            issue_links_response("OPEN", &[pr_number]),
            graphql_response(snapshot),
        ],
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
            state: "pr_open".to_string(),
        }
    );
    assert_recovered_binding(&store, &project_id, issue_number, pr_number, "pr_open").await?;
    Ok(())
}

#[tokio::test]
async fn ready_pr_recovery_transitions_existing_uncovered_workflow_before_reporting_coverage(
) -> anyhow::Result<()> {
    let Some((dir, store)) = open_runtime_store().await? else {
        return Ok(());
    };
    let project_root = dir.path().join("existing-planning");
    std::fs::create_dir(&project_root)?;
    let project_id = project_root.to_string_lossy().into_owned();
    let issue_number = 1_782;
    let pr_number = 1_783;
    store
        .upsert_definition(&WorkflowDefinition::new(
            GITHUB_ISSUE_PR_DEFINITION_ID,
            1,
            "GitHub issue PR workflow",
        ))
        .await?;
    let existing = WorkflowInstance::new(
        GITHUB_ISSUE_PR_DEFINITION_ID,
        1,
        "planning",
        WorkflowSubject::new("issue", format!("issue:{issue_number}")),
    )
    .with_id(workflow_id(&project_id, Some(REPO), issue_number));
    store.upsert_instance(&existing).await?;
    let rest_url = spawn_json_server("200 OK", vec![rest_candidates(&[])]).await;
    let graphql_url = spawn_json_server(
        "200 OK",
        vec![
            issue_links_response("OPEN", &[pr_number]),
            graphql_response(pr_snapshot(
                pr_number,
                issue_number,
                "OPEN",
                "SUCCESS",
                true,
            )),
        ],
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
            state: "quality_gate_pending".to_string(),
        }
    );
    assert_recovered_binding(
        &store,
        &project_id,
        issue_number,
        pr_number,
        "quality_gate_pending",
    )
    .await?;
    assert_quality_gate_queued(&store, &project_id, issue_number, pr_number).await?;
    Ok(())
}

#[tokio::test]
async fn recovery_uses_workflow_base_branch_for_later_merge_gates() -> anyhow::Result<()> {
    let Some((dir, store)) = open_runtime_store().await? else {
        return Ok(());
    };
    let project_root = dir.path().join("release-base");
    std::fs::create_dir(&project_root)?;
    std::fs::write(
        project_root.join("WORKFLOW.md"),
        "---\nbase:\n  branch: release\n---\n",
    )?;
    let project_id = project_root.to_string_lossy().into_owned();
    let issue_number = 1_784;
    let pr_number = 1_785;
    let rest_url = spawn_json_server("200 OK", vec![rest_candidates(&[])]).await;
    let graphql_url = spawn_json_server(
        "200 OK",
        vec![
            issue_links_response("OPEN", &[pr_number]),
            graphql_response(pr_snapshot(
                pr_number,
                issue_number,
                "OPEN",
                "SUCCESS",
                true,
            )),
        ],
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
            state: "awaiting_feedback".to_string(),
        }
    );
    let workflow = store
        .get_instance(&workflow_id(&project_id, Some(REPO), issue_number))
        .await?
        .expect("recovered workflow");
    assert_eq!(workflow.data["expected_base_ref"], "release");
    assert_eq!(
        workflow.data["recovered_pr_snapshot"]["expected_base_ref"],
        "release"
    );
    assert_no_agent_work(&store, &project_id, issue_number).await?;
    Ok(())
}
