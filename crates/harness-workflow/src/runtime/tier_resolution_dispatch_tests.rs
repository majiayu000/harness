use super::*;
use harness_core::{
    config::isolation::{IsolationConfig, IsolationRule, IsolationTier, IsolationTrustClass},
    db::resolve_database_url,
};
use serde_json::json;

#[tokio::test]
async fn tier_resolution_runtime_dispatch_records_isolation_evidence() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }

    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let instance = WorkflowInstance::new(
        GITHUB_ISSUE_PR_DEFINITION_ID,
        1,
        "implementing",
        WorkflowSubject::new("issue", "issue:42"),
    )
    .with_id("tier-resolution-workflow")
    .with_data(json!({
        "project_id": "/project",
        "repo": "owner/repo",
        "issue_number": 42,
        "author_trust_class": "non_collaborator",
    }));
    store.upsert_instance(&instance).await?;

    let command = WorkflowCommand::enqueue_activity("implement_issue", "tier-resolution-command");
    let command_id = store.enqueue_command(&instance.id, None, &command).await?;
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
    });

    let outcome = dispatcher
        .dispatch_once()
        .await?
        .expect("pending command should dispatch");
    let runtime_job = match outcome {
        CommandDispatchOutcome::Enqueued {
            command_id: dispatched_command_id,
            runtime_job,
        } => {
            assert_eq!(dispatched_command_id, command_id);
            runtime_job
        }
        other => panic!("unexpected dispatch outcome: {other:?}"),
    };

    assert_eq!(runtime_job.input["isolation"]["tier"], "container");
    assert_eq!(
        runtime_job.input["isolation"]["trust_class"],
        "non_collaborator"
    );
    assert!(runtime_job.input["isolation"]["reason"]
        .as_str()
        .is_some_and(|reason| reason.contains("matched configured isolation rule")));
    Ok(())
}
