use super::*;
use crate::workflow_runtime_submission::{
    cancel_submission_by_workflow_id, record_issue_submission, IssueSubmissionRuntimeContext,
    RuntimeSubmissionCancelOutcome,
};
use crate::workspace::test_support::{async_env_lock, ScopedEnvVar};
use harness_core::types::TaskId;

#[tokio::test]
async fn operator_cancel_survives_unlinked_pr_intake_and_allows_explicit_resubmission(
) -> anyhow::Result<()> {
    let _env_lock = async_env_lock().lock().await;
    let Some((dir, store)) = open_runtime_store().await? else {
        return Ok(());
    };
    let project_root = dir.path().join("operator-cancel");
    std::fs::create_dir(&project_root)?;
    let project_id = project_root.to_string_lossy().into_owned();
    let (issue_number, pr_number) = (1111, 1112);
    let linked = spawn_json_server(
        "200 OK",
        vec![
            issue_links_response("OPEN", &[pr_number]),
            graphql_response(pr_snapshot(
                pr_number,
                issue_number,
                "OPEN",
                "PENDING",
                false,
            )),
        ],
    )
    .await;
    recover_with_urls(
        &store,
        &project_root,
        &project_id,
        issue_number,
        "unused",
        &linked,
    )
    .await?;
    let id = workflow_id(&project_id, Some(REPO), issue_number);
    let recovered = store.get_instance(&id).await?.expect("recovered issue");

    // The PR loses its closing link before the operator cancels. A different
    // issue hash is not evidence of an external change after cancellation:
    // recovery stored a PR hash, whereas direct intake stores an issue hash.
    let issue = IncomingIssue {
        source: "github".to_string(),
        external_id: issue_number.to_string(),
        identifier: format!("#{issue_number}"),
        title: "Issue with a separately owned PR repair".to_string(),
        description: None,
        repo: Some(REPO.to_string()),
        url: Some(format!("https://github.com/{REPO}/issues/{issue_number}")),
        priority: None,
        labels: vec![],
        created_at: None,
        author_trust_class: IsolationTrustClass::Trusted,
        project_root: Some(project_root.clone()),
    };
    let issue_hash = record_issue_remote_fact_snapshot(Some(&store), REPO, issue_number, &issue)
        .await?
        .expect("issue fact hash");
    assert_ne!(recovered.data["last_remote_fact_hash"], issue_hash);
    let unlinked = spawn_json_server("200 OK", vec![issue_links_response("OPEN", &[])]).await;
    let _graphql = ScopedEnvVar::set("HARNESS_GITHUB_GRAPHQL_URL", &unlinked);
    assert!(matches!(
        cancel_submission_by_workflow_id(&store, &id).await?,
        RuntimeSubmissionCancelOutcome::Cancelled(_)
    ));
    let cancelled = store.get_instance(&id).await?.expect("cancelled issue");
    let event_count = store.events_for(&id).await?.len();
    let command_count = store.commands_for(&id).await?.len();

    // Follow the same coverage -> direct submission sequence as the poller.
    let task_id = TaskId::from_str(&format!("github-issue:{REPO}:issue:{issue_number}"));
    let context = |hash| IssueSubmissionRuntimeContext {
        project_root: &project_root,
        repo: Some(REPO),
        issue_number,
        task_id: &task_id,
        labels: &[],
        force_execute: false,
        additional_prompt: None,
        depends_on: &[],
        dependencies_blocked: false,
        source: Some("github"),
        external_id: Some("1111"),
        remote_fact_hash: hash,
        author_trust_class: Some(IsolationTrustClass::Trusted),
    };
    let coverage = check_github_issue_coverage(
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
    if coverage == GitHubIssueCoverage::Uncovered {
        record_issue_submission(&store, context(Some(&issue_hash))).await?;
    }
    let after_poll = store.get_instance(&id).await?.expect("issue after poll");
    assert_eq!(
        after_poll.state, "cancelled",
        "passive intake must retain operator cancellation"
    );
    assert_eq!(after_poll.version, cancelled.version);
    assert_eq!(store.events_for(&id).await?.len(), event_count);
    assert_eq!(store.commands_for(&id).await?.len(), command_count);
    assert_no_agent_work(&store, &project_id, issue_number).await?;

    // A delayed lookup returning a linked PR must also retain the operator
    // decision when it reaches the recovery persistence boundary.
    let delayed = spawn_json_server(
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
    assert_eq!(
        recover_with_urls(
            &store,
            &project_root,
            &project_id,
            issue_number,
            "unused",
            &delayed
        )
        .await?,
        GitHubIssueCoverage::Covered {
            source: "workflow_runtime",
            state: "cancelled".to_string()
        },
    );
    assert_no_agent_work(&store, &project_id, issue_number).await?;

    let explicit = record_issue_submission(&store, context(None)).await?;
    assert!(explicit.accepted);
    assert_eq!(explicit.command_ids.len(), 1);
    assert_eq!(
        store
            .get_instance(&id)
            .await?
            .expect("resubmitted issue")
            .state,
        "planning"
    );
    assert_eq!(store.events_for(&id).await?.len(), event_count + 1);
    Ok(())
}

#[tokio::test]
async fn remote_closed_pr_cancellation_allows_later_reopened_pr_observation() -> anyhow::Result<()>
{
    let Some((dir, store)) = open_runtime_store().await? else {
        return Ok(());
    };
    let project_root = dir.path().join("remote-closed");
    std::fs::create_dir(&project_root)?;
    let project_id = project_root.to_string_lossy().into_owned();
    let (issue_number, pr_number) = (1113, 1114);
    let candidate = ClosingPullRequestCandidate {
        number: pr_number,
        url: format!("https://github.com/{REPO}/pull/{pr_number}"),
        head_ref_name: "repair".to_string(),
        repo_slug: REPO.to_string(),
    };
    let closed = json!({"state": "CLOSED", "head_oid": "old-head", "expected_base_ref": "main"});
    let fact = RemoteFactSnapshot::new(
        "github",
        REPO,
        "pull_request",
        pr_number as i64,
        "closed",
        closed.clone(),
        Utc::now(),
    );
    assert_eq!(
        persist_recovered_workflow(
            &store,
            &project_root,
            &project_id,
            REPO,
            issue_number,
            &candidate,
            &closed,
            &fact,
            "cancelled",
            IsolationTrustClass::Trusted,
            None
        )
        .await?,
        RecoveredWorkflowPersistence::Persisted,
    );
    let reopened = spawn_json_server(
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
    assert_eq!(
        recover_with_urls(
            &store,
            &project_root,
            &project_id,
            issue_number,
            "unused",
            &reopened
        )
        .await?,
        GitHubIssueCoverage::Covered {
            source: "github_closing_pr",
            state: "quality_gate_pending".to_string()
        },
    );
    assert_quality_gate_queued(&store, &project_id, issue_number, pr_number).await?;
    Ok(())
}
