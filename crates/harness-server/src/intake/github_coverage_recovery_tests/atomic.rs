use super::*;
use crate::workflow_runtime_submission::{
    cancel_submission_by_workflow_id, record_issue_submission, IssueSubmissionRuntimeContext,
    RuntimeSubmissionCancelOutcome,
};
use crate::workspace::test_support::{async_env_lock, ScopedEnvVar};
use harness_core::config::isolation::{
    IsolationAvailability, IsolationConfig, IsolationRule, IsolationTier, IsolationTierStatus,
    IsolationTrustClass,
};
use harness_core::types::TaskId;
use harness_workflow::issue_lifecycle::IssueWorkflowInstance;
use harness_workflow::project_lifecycle::{ProjectWorkflowInstance, ProjectWorkflowStore};
use harness_workflow::runtime::{
    CommandDispatchOutcome, RuntimeCommandDispatcher, RuntimeKind, RuntimeProfile,
    WorkflowCommandStatus,
};

#[tokio::test]
async fn canonical_intake_reuses_legacy_issue_and_project_workflows() -> anyhow::Result<()> {
    let Some(database_url) = crate::test_helpers::configured_test_database_url()? else {
        return Ok(());
    };
    let dir = crate::test_helpers::tempdir_in_home("harness-test-legacy-intake-")?;
    let issue_store = IssueWorkflowStore::open_with_database_url(
        &dir.path().join("issue-workflows"),
        Some(&database_url),
    )
    .await?;
    let project_store = ProjectWorkflowStore::open_with_database_url(
        &dir.path().join("project-workflows"),
        Some(&database_url),
    )
    .await?;
    let project_root = dir.path().join("project");
    std::fs::create_dir(&project_root)?;
    let project_id = project_root.to_string_lossy().into_owned();
    let issue_number = 1708;
    let mut legacy_issue = IssueWorkflowInstance::new(
        project_id.clone(),
        Some("owner/repo".to_string()),
        issue_number,
    );
    legacy_issue.id = workflow_id(&project_id, Some("Owner/Repo"), issue_number);
    legacy_issue.repo = Some("Owner/Repo".to_string());
    issue_store.upsert(&legacy_issue).await?;
    let mut legacy_project =
        ProjectWorkflowInstance::new(project_id.clone(), Some("owner/repo".to_string()));
    legacy_project.id =
        harness_workflow::project_lifecycle::workflow_id(&project_id, Some("Owner/Repo"));
    legacy_project.repo = Some("Owner/Repo".to_string());
    project_store.upsert(&legacy_project).await?;

    let coverage = check_github_issue_coverage(
        Some(&issue_store),
        None,
        &project_root,
        &project_id,
        REPO,
        issue_number,
        IsolationTrustClass::Trusted,
        None,
    )
    .await?;
    assert!(matches!(
        coverage,
        GitHubIssueCoverage::Covered {
            source: "issue_workflow",
            ..
        }
    ));
    let idle = project_store.record_idle(&project_id, Some(REPO)).await?;

    assert_eq!(idle.id, legacy_project.id);
    assert_eq!(project_store.row_count().await?, 1);
    assert_eq!(issue_store.row_count().await?, 1);
    assert_eq!(
        issue_store
            .get_by_issue(&project_id, Some(REPO), issue_number)
            .await?
            .map(|workflow| workflow.id),
        Some(legacy_issue.id)
    );
    Ok(())
}

#[tokio::test]
async fn cancelled_recovered_quality_gate_is_reactivated_after_terminal_job() -> anyhow::Result<()>
{
    let Some((dir, store)) = open_runtime_store().await? else {
        return Ok(());
    };
    let project_root = dir.path().join("cancelled-quality-gate");
    std::fs::create_dir(&project_root)?;
    let project_id = project_root.to_string_lossy().into_owned();
    let (issue_number, pr_number) = (1_790, 1_791);
    let first_graphql = ready_pr_server(issue_number, pr_number).await;
    recover_with_urls(
        &store,
        &project_root,
        &project_id,
        issue_number,
        "unused",
        &first_graphql,
    )
    .await?;
    let workflow_id = workflow_id(&project_id, Some(REPO), issue_number);
    let command = store
        .commands_for(&workflow_id)
        .await?
        .into_iter()
        .find(|record| record.command.requires_runtime_job())
        .ok_or_else(|| anyhow::anyhow!("quality gate command was not queued"))?;
    store
        .enqueue_runtime_job_for_pending_command(
            &command.id,
            RuntimeKind::CodexJsonrpc,
            "codex-default",
            json!({"activity": "start_child_workflow"}),
            None,
        )
        .await?;
    store
        .cancel_command_and_unfinished_runtime_jobs(&command.id, "test", "cancel before recovery")
        .await?;
    let mut parent = store.get_instance(&workflow_id).await?.expect("parent");
    parent.state = "cancelled".to_string();
    parent.version += 1;
    crate::test_helpers::force_upsert_runtime_lifecycle_state_for_test(&store, &parent).await?;

    let second_graphql = ready_pr_server(issue_number, pr_number).await;
    recover_with_urls(
        &store,
        &project_root,
        &project_id,
        issue_number,
        "unused",
        &second_graphql,
    )
    .await?;
    let commands = store.commands_for(&workflow_id).await?;
    assert!(commands.iter().all(|record| {
        record.command.requires_runtime_job()
            || record.status == WorkflowCommandStatus::HandledInline
    }));
    let runtime_commands = commands
        .iter()
        .filter(|record| record.command.requires_runtime_job())
        .collect::<Vec<_>>();
    // Recovery re-enqueues the cancelled command as a new attempt rather than
    // resurrecting the cancelled row, so the cancellation stays in the history
    // and the retry carries its own identity (GH-1865).
    assert_eq!(runtime_commands.len(), 2);
    let superseded = runtime_commands
        .iter()
        .find(|record| record.id == command.id)
        .expect("the cancelled attempt must survive recovery");
    assert_eq!(superseded.status, WorkflowCommandStatus::Superseded);
    assert_eq!(superseded.attempt_generation, 1);
    let retried = runtime_commands
        .iter()
        .find(|record| record.id != command.id)
        .expect("recovery must queue a new attempt");
    assert_eq!(retried.status, WorkflowCommandStatus::Pending);
    assert_eq!(retried.attempt_generation, 2);
    assert_eq!(
        superseded.superseded_by_command_id.as_deref(),
        Some(retried.id.as_str())
    );
    store
        .enqueue_runtime_job_for_pending_command(
            &retried.id,
            RuntimeKind::CodexJsonrpc,
            "codex-default",
            json!({"activity": "start_child_workflow"}),
            None,
        )
        .await?;
    // Each attempt owns its own runtime job; the cancelled one keeps its.
    assert_eq!(store.runtime_jobs_for_command(&retried.id).await?.len(), 1);
    assert_eq!(store.runtime_jobs_for_command(&command.id).await?.len(), 1);
    Ok(())
}

#[tokio::test]
async fn recovered_non_collaborator_quality_gate_dispatches_in_container() -> anyhow::Result<()> {
    let Some((dir, store)) = open_runtime_store().await? else {
        return Ok(());
    };
    let project_root = dir.path().join("non-collaborator-quality-gate");
    std::fs::create_dir(&project_root)?;
    let project_id = project_root.to_string_lossy().into_owned();
    let (issue_number, pr_number) = (1_792, 1_793);
    let graphql_url = ready_pr_server(issue_number, pr_number).await;
    recover_with_urls_and_trust(
        &store,
        &project_root,
        &project_id,
        issue_number,
        &graphql_url,
        IsolationTrustClass::NonCollaborator,
    )
    .await?;
    let recovered = store
        .get_instance(&workflow_id(&project_id, Some(REPO), issue_number))
        .await?
        .expect("recovered workflow");
    assert_eq!(recovered.data["author_trust_class"], "non_collaborator");
    let dispatcher = RuntimeCommandDispatcher::new(
        &store,
        RuntimeProfile::new("codex-default", RuntimeKind::CodexJsonrpc),
    )
    .with_isolation_config(IsolationConfig {
        default_tier: IsolationTier::Host,
        rules: vec![IsolationRule {
            trust: IsolationTrustClass::NonCollaborator,
            tier: IsolationTier::Container,
        }],
        network_allowlist: Vec::new(),
    })
    .with_isolation_availability(IsolationAvailability::new(vec![
        IsolationTierStatus::available(IsolationTier::Host),
        IsolationTierStatus::available(IsolationTier::Container),
    ]));
    let runtime_job = match dispatcher
        .dispatch_once()
        .await?
        .expect("quality gate command")
    {
        CommandDispatchOutcome::Enqueued { runtime_job, .. } => runtime_job,
        other => panic!("unexpected dispatch outcome: {other:?}"),
    };
    assert_eq!(runtime_job.input["isolation"]["tier"], "container");
    assert_eq!(
        runtime_job.input["isolation"]["trust_class"],
        "non_collaborator"
    );
    Ok(())
}

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

async fn ready_pr_server(issue_number: u64, pr_number: u64) -> String {
    spawn_json_server(
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
    .await
}
