use super::*;
use harness_core::{
    config::isolation::{
        IsolationAvailability, IsolationConfig, IsolationRule, IsolationTier, IsolationTierStatus,
        IsolationTrustClass,
    },
    db::resolve_database_url,
};
use serde_json::json;

async fn dispatch_without_local_isolation(
    runtime_kind: RuntimeKind,
    eval_isolation: Option<serde_json::Value>,
) -> anyhow::Result<CommandDispatchOutcome> {
    let dir = tempfile::tempdir()?;
    let store = WorkflowRuntimeStore::open(&dir.path().join("workflow_runtime.db")).await?;
    let instance = WorkflowInstance::new(
        GITHUB_ISSUE_PR_DEFINITION_ID,
        1,
        "implementing",
        WorkflowSubject::new("issue", "issue:42"),
    )
    .with_id(format!("remote-isolation-{}", uuid::Uuid::new_v4()))
    .with_server_data(json!({
        "project_id": "/project",
        "repo": "owner/repo",
        "issue_number": 42,
        "author_trust_class": "non_collaborator",
    }));
    store
        .force_upsert_lifecycle_state_for_test(&instance)
        .await?;
    let mut command = WorkflowCommand::enqueue_activity("implement_issue", &instance.id);
    if let Some(isolation) = eval_isolation {
        command.command["eval"] = json!({"timeout_secs": 45, "isolation": isolation});
    }
    store.enqueue_command(&instance.id, None, &command).await?;
    let dispatcher = RuntimeCommandDispatcher::new(
        &store,
        RuntimeProfile::new("unavailable-control-plane", runtime_kind),
    )
    .with_isolation_config(IsolationConfig {
        default_tier: IsolationTier::Container,
        rules: Vec::new(),
        network_allowlist: vec!["chatgpt.com".to_string()],
    })
    .with_isolation_availability(IsolationAvailability::new(vec![
        IsolationTierStatus::unavailable(IsolationTier::Host, "control plane has no sandbox"),
        IsolationTierStatus::unavailable(IsolationTier::Container, "control plane has no Docker"),
    ]));
    dispatcher
        .dispatch_once()
        .await?
        .ok_or_else(|| anyhow::anyhow!("pending command should be claimed"))
}

fn remote_eval_isolation() -> serde_json::Value {
    json!({
        "tier": "container",
        "runtime_kind": "remote_host",
        "runtime_profile": "eval-isolated-runtime-host",
        "sandbox": "workspace-write",
        "backend": "container_runtime_host",
        "image": "harness-eval-runner:local",
        "lifecycle": "ephemeral",
        "cleanup_required": true,
        "network_allowlist": ["chatgpt.com"],
    })
}

#[tokio::test]
async fn tier_resolution_remote_dispatch_preserves_contract_without_local_isolation(
) -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }
    for eval in [None, Some(remote_eval_isolation())] {
        let outcome =
            dispatch_without_local_isolation(RuntimeKind::RemoteHost, eval.clone()).await?;
        let CommandDispatchOutcome::Enqueued { runtime_job, .. } = outcome else {
            panic!("remote host should not require a control-plane sandbox: {outcome:?}");
        };
        assert_eq!(runtime_job.runtime_kind, RuntimeKind::RemoteHost);
        assert_eq!(runtime_job.input["isolation"]["tier"], "container");
        assert_eq!(
            runtime_job.input["isolation"]["trust_class"],
            "non_collaborator"
        );
        assert_eq!(
            runtime_job.input["isolation"]["network_allowlist"],
            json!(["chatgpt.com"])
        );
        if let Some(eval) = eval {
            assert_eq!(runtime_job.input["command"]["eval"]["isolation"], eval);
            assert_eq!(runtime_job.runtime_profile, "eval-isolated-runtime-host");
        }
    }
    Ok(())
}

#[tokio::test]
async fn tier_resolution_local_dispatch_still_requires_local_isolation() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }
    let outcome = dispatch_without_local_isolation(RuntimeKind::CodexJsonrpc, None).await?;
    let CommandDispatchOutcome::Deferred { barrier, .. } = outcome else {
        panic!("local execution must remain deferred: {outcome:?}");
    };
    assert_eq!(
        barrier.reason_code,
        DispatchBarrierReasonCode::IsolationTierUnavailable
    );
    assert_eq!(barrier.required_tier.as_deref(), Some("container"));
    assert!(barrier.reason.contains("control plane has no Docker"));
    Ok(())
}

#[tokio::test]
async fn tier_resolution_remote_dispatch_rejects_invalid_eval_isolation() -> anyhow::Result<()> {
    if resolve_database_url(None).is_err() {
        return Ok(());
    }
    for (field, value, expected_error) in [
        ("tier", json!("host"), "not valid for untrusted eval cases"),
        ("tier", json!("microvm"), "reserved but not implemented"),
        (
            "network_allowlist",
            json!(["0x7f.0.0.1"]),
            "invalid eval network allowlist hostname `0x7f.0.0.1`",
        ),
    ] {
        let mut isolation = remote_eval_isolation();
        isolation[field] = value;
        let error = dispatch_without_local_isolation(RuntimeKind::RemoteHost, Some(isolation))
            .await
            .expect_err("remote dispatch must still validate its isolation contract");
        assert!(format!("{error:#}").contains(expected_error), "{error:#}");
    }
    Ok(())
}

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
    .with_server_data(json!({
        "project_id": "/project",
        "repo": "owner/repo",
        "issue_number": 42,
        "author_trust_class": "non_collaborator",
    }));
    store
        .force_upsert_lifecycle_state_for_test(&instance)
        .await?;

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
    })
    .with_isolation_availability(IsolationAvailability::new(vec![
        IsolationTierStatus::available(IsolationTier::Host),
        IsolationTierStatus::available(IsolationTier::Container),
    ]));

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
