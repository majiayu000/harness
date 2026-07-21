use super::*;
use harness_core::config::isolation::{
    IsolationAvailability, IsolationConfig, IsolationRule, IsolationTier, IsolationTierStatus,
    IsolationTrustClass,
};
use harness_workflow::runtime::{
    CommandDispatchOutcome, RuntimeCommandDispatcher, RuntimeKind, RuntimeProfile,
    WorkflowCommandStatus,
};

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
    let command = store.commands_for(&workflow_id).await?.remove(0);
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
    store.upsert_instance(&parent).await?;

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
    assert_eq!(commands.len(), 1);
    assert_eq!(commands[0].id, command.id);
    assert_eq!(commands[0].status, WorkflowCommandStatus::Pending);
    store
        .enqueue_runtime_job_for_pending_command(
            &command.id,
            RuntimeKind::CodexJsonrpc,
            "codex-default",
            json!({"activity": "start_child_workflow"}),
            None,
        )
        .await?;
    assert_eq!(store.runtime_jobs_for_command(&command.id).await?.len(), 2);
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
