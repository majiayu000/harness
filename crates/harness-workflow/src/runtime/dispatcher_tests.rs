use super::*;
use crate::runtime::{RuntimeKind, WorkflowCommand, WorkflowCommandStatus, WorkflowCommandType};

#[test]
fn profile_selector_uses_default_profile_without_activity_override() {
    let mut default_profile = RuntimeProfile::new("codex-default", RuntimeKind::CodexJsonrpc);
    default_profile.model = Some("gpt-5.5".to_string());
    default_profile.reasoning_effort = Some("high".to_string());

    let selector = RuntimeProfileSelector::new(default_profile);
    let profile = selector.select(Some("github_issue_pr"), Some("implement_issue"));

    assert_eq!(profile.kind, RuntimeKind::CodexJsonrpc);
    assert_eq!(profile.name, "codex-default");
    assert_eq!(profile.model.as_deref(), Some("gpt-5.5"));
    assert_eq!(profile.reasoning_effort.as_deref(), Some("high"));
    assert_eq!(profile.timeout_secs, None);
}

#[test]
fn candidate_fanout_budget_splits_runtime_profile_max_turns() -> anyhow::Result<()> {
    let mut profile = RuntimeProfile::new("codex-default", RuntimeKind::CodexJsonrpc);
    profile.max_turns = Some(9);
    let payload = json!({
        "candidate": {
            "candidate_count": 3,
            "budget": { "max_turns_per_candidate": null },
        },
    });

    apply_candidate_runtime_budget(&mut profile, &payload)?;

    assert_eq!(profile.max_turns, Some(3));
    Ok(())
}

#[test]
fn candidate_fanout_budget_override_wins_over_split() -> anyhow::Result<()> {
    let mut profile = RuntimeProfile::new("codex-default", RuntimeKind::CodexJsonrpc);
    profile.max_turns = Some(9);
    let payload = json!({
        "candidate": {
            "candidate_count": 3,
            "budget": { "max_turns_per_candidate": 5 },
        },
    });

    apply_candidate_runtime_budget(&mut profile, &payload)?;

    assert_eq!(profile.max_turns, Some(5));
    Ok(())
}

#[test]
fn profile_selector_allows_explicit_activity_override() {
    let default_profile = RuntimeProfile::new("codex-default", RuntimeKind::CodexJsonrpc);
    let mut override_profile = RuntimeProfile::new("custom-feedback", RuntimeKind::ClaudeCode);
    override_profile.timeout_secs = Some(7200);

    let selector = RuntimeProfileSelector::new(default_profile)
        .with_activity_profile("address_pr_feedback", override_profile);
    let profile = selector.select(Some("github_issue_pr"), Some("address_pr_feedback"));

    assert_eq!(profile.kind, RuntimeKind::ClaudeCode);
    assert_eq!(profile.name, "custom-feedback");
    assert_eq!(profile.timeout_secs, Some(7200));
}

#[test]
fn remote_merge_profile_is_forced_onto_the_local_server_worker() -> anyhow::Result<()> {
    let instance = WorkflowInstance::new(
        crate::runtime::GITHUB_ISSUE_PR_DEFINITION_ID,
        crate::runtime::GITHUB_ISSUE_PR_DEFINITION_VERSION,
        "merging",
        crate::runtime::WorkflowSubject::new("issue", "issue:77"),
    )
    .with_server_data(json!({
        "definition_hash": crate::runtime::github_issue_pr_definition_hash(),
        "merge_execution": "server"
    }));
    let mut profile = RuntimeProfile::new("remote-merge", RuntimeKind::RemoteHost);
    profile.model = Some("remote-model".to_string());

    super::super::dispatcher_throttle::force_server_owned_profile(
        &crate::runtime::WorkflowDefinitionRegistry::default(),
        Some(&instance),
        "merge_pr",
        &mut profile,
    )?;

    assert_eq!(profile.kind, RuntimeKind::CodexExec);
    assert_eq!(profile.name, "server-owned-merge");
    assert_eq!(profile.model, None);
    Ok(())
}

#[test]
fn agent_merge_profile_is_not_intercepted() -> anyhow::Result<()> {
    let instance = WorkflowInstance::new(
        crate::runtime::GITHUB_ISSUE_PR_DEFINITION_ID,
        crate::runtime::GITHUB_ISSUE_PR_DEFINITION_VERSION,
        "merging",
        crate::runtime::WorkflowSubject::new("issue", "issue:78"),
    )
    .with_server_data(json!({
        "definition_hash": crate::runtime::github_issue_pr_definition_hash(),
        "merge_execution": "agent"
    }));
    let mut profile = RuntimeProfile::new("remote-merge", RuntimeKind::RemoteHost);

    super::super::dispatcher_throttle::force_server_owned_profile(
        &crate::runtime::WorkflowDefinitionRegistry::default(),
        Some(&instance),
        "merge_pr",
        &mut profile,
    )?;

    assert_eq!(profile.kind, RuntimeKind::RemoteHost);
    assert_eq!(profile.name, "remote-merge");
    Ok(())
}

#[test]
fn remote_classifier_profile_is_rejected_before_dispatch() {
    let registry = crate::runtime::WorkflowDefinitionRegistry::with_builtins();
    let instance = WorkflowInstance::new(
        crate::runtime::GITHUB_ISSUE_PR_DEFINITION_ID,
        crate::runtime::GITHUB_ISSUE_PR_DEFINITION_VERSION,
        "pr_scope_review",
        crate::runtime::WorkflowSubject::new("issue", "issue:77"),
    )
    .with_server_data(json!({
        "definition_hash": crate::runtime::github_issue_pr_definition_hash()
    }));
    let mut profile = RuntimeProfile::new("remote-classifier", RuntimeKind::RemoteHost);

    let error = super::super::dispatcher_throttle::force_server_owned_profile(
        &registry,
        Some(&instance),
        crate::runtime::CHANGE_SCOPE_REVIEW_ACTIVITY,
        &mut profile,
    )
    .expect_err("remote hosts must not produce trusted classifier assessments");

    assert!(error.to_string().contains("must use a local agent runtime"));
}

#[test]
fn eval_isolation_command_policy_overrides_host_defaults() -> anyhow::Result<()> {
    let command = command_record(WorkflowCommand::new(
        WorkflowCommandType::EnqueueActivity,
        "eval-implement",
        json!({
            "activity": "implement_issue",
            "eval": {
                "timeout_secs": 1800,
                "isolation": {
                    "tier": "container",
                    "runtime_kind": "remote_host",
                    "runtime_profile": "eval-isolated-runtime-host",
                    "sandbox": "workspace-write",
                    "backend": "container_runtime_host",
                    "image": "harness-eval-runner:local",
                    "lifecycle": "ephemeral",
                    "cleanup_required": true
                }
            }
        }),
    ));

    let resolution = isolation_resolution_for_command(None, &command, &IsolationConfig::default())?;

    assert_eq!(resolution.tier, IsolationTier::Container);
    assert_eq!(resolution.trust_class, IsolationTrustClass::NonCollaborator);
    assert!(resolution.reason.contains("eval command required"));
    Ok(())
}

#[test]
fn eval_isolation_command_policy_selects_remote_host_profile() -> anyhow::Result<()> {
    let command = command_record(WorkflowCommand::new(
        WorkflowCommandType::EnqueueActivity,
        "eval-implement",
        json!({
            "activity": "implement_issue",
            "eval": {
                "timeout_secs": 1800,
                "isolation": {
                    "tier": "container",
                    "runtime_kind": "remote_host",
                    "runtime_profile": "eval-isolated-runtime-host",
                    "sandbox": "workspace-write",
                    "backend": "container_runtime_host",
                    "image": "harness-eval-runner:local",
                    "lifecycle": "ephemeral",
                    "cleanup_required": true
                }
            }
        }),
    ));
    let mut profile = RuntimeProfile::new("codex-default", RuntimeKind::CodexJsonrpc);

    apply_eval_runtime_profile_policy(&mut profile, &command)?;

    assert_eq!(profile.kind, RuntimeKind::RemoteHost);
    assert_eq!(profile.name, "eval-isolated-runtime-host");
    assert_eq!(profile.sandbox.as_deref(), Some("workspace-write"));
    assert_eq!(profile.timeout_secs, Some(1800));
    assert_eq!(profile.model, None);
    assert_eq!(profile.reasoning_effort, None);
    Ok(())
}

#[test]
fn eval_isolation_command_policy_rejects_host_tier() {
    let command = command_record(WorkflowCommand::new(
        WorkflowCommandType::EnqueueActivity,
        "eval-implement",
        json!({
            "activity": "implement_issue",
            "eval": { "isolation": { "tier": "host" } }
        }),
    ));

    let error = isolation_resolution_for_command(None, &command, &IsolationConfig::default())
        .expect_err("host eval isolation must fail");

    assert!(error.to_string().contains("requested host isolation"));
}

fn command_record(command: WorkflowCommand) -> WorkflowCommandRecord {
    WorkflowCommandRecord {
        id: "command-1".to_string(),
        workflow_id: "workflow-1".to_string(),
        decision_id: None,
        status: WorkflowCommandStatus::Pending,
        dispatch_owner: None,
        dispatch_lease_expires_at: None,
        dispatch_not_before: None,
        dispatch_attempt_count: 0,
        dispatch_claim_generation: 0,
        dispatch_barrier: None,
        command,
        created_at: Utc::now(),
        updated_at: Utc::now(),
        attempt_generation: 1,
        superseded_by_command_id: None,
    }
}
