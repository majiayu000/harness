use super::*;

const RECONCILE_GH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

#[test]
fn background_review_config_for_request_applies_project_override() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let harness_dir = dir.path().join(".harness");
    std::fs::create_dir(&harness_dir)?;
    std::fs::write(
        harness_dir.join("config.toml"),
        r#"
                [review]
                enabled = true
                review_bot_auto_trigger = false
            "#,
    )?;
    let mut server_config = harness_core::config::HarnessConfig::default();
    server_config.agents.review.enabled = false;
    server_config.agents.review.review_bot_auto_trigger = true;
    let req = task_runner::CreateTaskRequest {
        project: Some(dir.path().to_path_buf()),
        ..Default::default()
    };

    let review_config = review_config_for_request(&server_config, &req)?;

    assert!(review_config.enabled);
    assert!(!review_config.review_bot_auto_trigger);
    assert_eq!(review_config.reviewer_agent, "codex");
    Ok(())
}

#[test]
fn runtime_dispatch_profile_selector_inherits_configured_agent_profile_when_unset() {
    let mut config = harness_core::config::HarnessConfig::default();
    config.agents.codex.default_model = "gpt-5.5".to_string();
    config.agents.codex.reasoning_effort = "xhigh".to_string();
    let inherited_profile = runtime_profile_from_agent(&config, "codex").unwrap();
    let policy = harness_core::config::workflow::RuntimeDispatchPolicy {
        approval_policy: Some("never".to_string()),
        ..Default::default()
    };

    let selector = runtime_dispatch_profile_selector(&config, &policy, &inherited_profile).unwrap();
    let profile = selector.select_for_workflow(Some("github_issue_pr"));
    assert_eq!(profile.kind, RuntimeKind::CodexJsonrpc);
    assert_eq!(profile.name, "codex-default");
    assert_eq!(profile.model.as_deref(), Some("gpt-5.5"));
    assert_eq!(profile.reasoning_effort.as_deref(), Some("xhigh"));
    assert_eq!(profile.approval_policy.as_deref(), Some("never"));
}

#[test]
fn runtime_dispatch_profile_explicit_kind_uses_matching_provider_config() {
    let mut config = harness_core::config::HarnessConfig::default();
    config.agents.codex.default_model = "gpt-5.5".to_string();
    config.agents.codex.reasoning_effort = "xhigh".to_string();
    config.agents.claude.default_model = "sonnet-explicit".to_string();
    let inherited_profile = runtime_profile_from_agent(&config, "codex").unwrap();
    let policy = harness_core::config::workflow::RuntimeDispatchPolicy {
        runtime_kind: Some("claude_code".to_string()),
        ..Default::default()
    };

    let profile = runtime_dispatch_profile(&config, &policy, &inherited_profile).unwrap();
    assert_eq!(profile.kind, RuntimeKind::ClaudeCode);
    assert_eq!(profile.name, "claude-default");
    assert_eq!(profile.model.as_deref(), Some("sonnet-explicit"));
    assert_eq!(profile.reasoning_effort, None);
}

#[test]
fn runtime_dispatch_profile_override_kind_uses_matching_provider_config() {
    let mut config = harness_core::config::HarnessConfig::default();
    config.agents.codex.default_model = "gpt-5.5".to_string();
    config.agents.codex.reasoning_effort = "xhigh".to_string();
    config.agents.claude.default_model = "sonnet-override".to_string();
    let inherited_profile = runtime_profile_from_agent(&config, "codex").unwrap();
    let mut policy = harness_core::config::workflow::RuntimeDispatchPolicy {
        runtime_kind: Some("codex_jsonrpc".to_string()),
        timeout_secs: Some(3600),
        ..Default::default()
    };
    policy.workflow_profiles.insert(
        "claude_review".to_string(),
        harness_core::config::workflow::RuntimeDispatchProfileOverride {
            runtime_kind: Some("claude_code".to_string()),
            ..Default::default()
        },
    );

    let selector = runtime_dispatch_profile_selector(&config, &policy, &inherited_profile).unwrap();
    let profile = selector.select_for_workflow(Some("claude_review"));
    assert_eq!(profile.kind, RuntimeKind::ClaudeCode);
    assert_eq!(profile.name, "claude-default");
    assert_eq!(profile.model.as_deref(), Some("sonnet-override"));
    assert_eq!(profile.reasoning_effort, None);
    assert_eq!(profile.timeout_secs, Some(3600));
}

#[test]
fn runtime_dispatch_profile_selector_applies_workflow_overrides() {
    let config = harness_core::config::HarnessConfig::default();
    let inherited_profile = RuntimeProfile::new("inherited-default", RuntimeKind::CodexJsonrpc);
    let mut policy = harness_core::config::workflow::RuntimeDispatchPolicy {
        runtime_kind: Some("claude_code".to_string()),
        runtime_profile: Some("claude-default".to_string()),
        model: Some("claude-sonnet-4-6".to_string()),
        timeout_secs: Some(600),
        ..Default::default()
    };
    policy.workflow_profiles.insert(
        "github_issue_pr".to_string(),
        harness_core::config::workflow::RuntimeDispatchProfileOverride {
            runtime_kind: Some("codex_exec".to_string()),
            runtime_profile: Some("codex-high".to_string()),
            model: Some("gpt-5.4".to_string()),
            timeout_secs: Some(1200),
            ..Default::default()
        },
    );
    policy.workflow_profiles.insert(
        "prompt_task".to_string(),
        harness_core::config::workflow::RuntimeDispatchProfileOverride {
            runtime_kind: None,
            runtime_profile: Some("codex-prompt-task".to_string()),
            ..Default::default()
        },
    );
    policy.activity_profiles.insert(
        "replan_issue".to_string(),
        harness_core::config::workflow::RuntimeDispatchProfileOverride {
            runtime_kind: Some("codex_jsonrpc".to_string()),
            runtime_profile: Some("codex-replan".to_string()),
            model: Some("gpt-5.4-mini".to_string()),
            ..Default::default()
        },
    );
    policy.workflow_activity_profiles.insert(
        "github_issue_pr".to_string(),
        std::collections::BTreeMap::from([(
            "replan_issue".to_string(),
            harness_core::config::workflow::RuntimeDispatchProfileOverride {
                runtime_profile: Some("codex-issue-replan".to_string()),
                model: Some("gpt-5.5".to_string()),
                timeout_secs: Some(900),
                ..Default::default()
            },
        )]),
    );

    let selector = runtime_dispatch_profile_selector(&config, &policy, &inherited_profile).unwrap();
    let issue_profile = selector.select_for_workflow(Some("github_issue_pr"));
    assert_eq!(issue_profile.kind, RuntimeKind::CodexExec);
    assert_eq!(issue_profile.name, "codex-high");
    assert_eq!(issue_profile.model.as_deref(), Some("gpt-5.4"));
    assert_eq!(issue_profile.timeout_secs, Some(1200));
    let replan_profile = selector.select(Some("github_issue_pr"), Some("replan_issue"));
    assert_eq!(replan_profile.kind, RuntimeKind::CodexJsonrpc);
    assert_eq!(replan_profile.name, "codex-issue-replan");
    assert_eq!(replan_profile.model.as_deref(), Some("gpt-5.5"));
    assert_eq!(replan_profile.timeout_secs, Some(900));
    let global_replan_profile = selector.select(Some("prompt_task"), Some("replan_issue"));
    assert_eq!(global_replan_profile.kind, RuntimeKind::CodexJsonrpc);
    assert_eq!(global_replan_profile.name, "codex-replan");
    assert_eq!(global_replan_profile.model.as_deref(), Some("gpt-5.4-mini"));
    assert_eq!(global_replan_profile.timeout_secs, Some(600));
    let prompt_task_profile = selector.select_for_workflow(Some("prompt_task"));
    assert_eq!(prompt_task_profile.kind, RuntimeKind::ClaudeCode);
    assert_eq!(prompt_task_profile.name, "codex-prompt-task");
    assert_eq!(
        prompt_task_profile.model.as_deref(),
        Some("claude-sonnet-4-6")
    );
    assert_eq!(prompt_task_profile.timeout_secs, Some(600));
    let default_profile = selector.select_for_workflow(Some("quality_gate"));
    assert_eq!(default_profile.kind, RuntimeKind::ClaudeCode);
    assert_eq!(default_profile.name, "claude-default");
    assert_eq!(default_profile.model.as_deref(), Some("claude-sonnet-4-6"));
    assert_eq!(default_profile.timeout_secs, Some(600));
}

#[test]
fn runtime_profile_manifest_definition_records_resolved_profiles() -> anyhow::Result<()> {
    let config = harness_core::config::HarnessConfig::default();
    let inherited_profile = RuntimeProfile::new("inherited-default", RuntimeKind::CodexJsonrpc);
    let mut policy = harness_core::config::workflow::RuntimeDispatchPolicy {
        runtime_kind: Some("codex_jsonrpc".to_string()),
        runtime_profile: Some("codex-default".to_string()),
        model: Some("gpt-default".to_string()),
        timeout_secs: Some(600),
        ..Default::default()
    };
    policy.workflow_activity_profiles.insert(
        "github_issue_pr".to_string(),
        std::collections::BTreeMap::from([(
            "replan_issue".to_string(),
            harness_core::config::workflow::RuntimeDispatchProfileOverride {
                runtime_profile: Some("codex-issue-replan".to_string()),
                model: Some("gpt-replan".to_string()),
                timeout_secs: Some(120),
                ..Default::default()
            },
        )]),
    );

    let project_root = std::path::Path::new("/tmp/harness-runtime-profile-project");
    let definition =
        runtime_profile_manifest_definition(project_root, &config, &policy, &inherited_profile)?;

    assert!(definition.id.starts_with("runtime_profiles:"));
    assert_eq!(definition.name, "Runtime Profile Manifest");
    assert_eq!(
        definition.source_path.as_deref(),
        Some("/tmp/harness-runtime-profile-project/WORKFLOW.md")
    );
    assert_eq!(
        definition.metadata["artifact_type"],
        "runtime_profile_manifest"
    );
    assert_eq!(
        definition.metadata["default_profile"]["name"],
        "codex-default"
    );
    assert_eq!(
        definition.metadata["workflow_activity_profiles"]["github_issue_pr"]["replan_issue"]
            ["name"],
        "codex-issue-replan"
    );
    assert_eq!(
        definition.metadata["workflow_activity_profiles"]["github_issue_pr"]["replan_issue"]
            ["model"],
        "gpt-replan"
    );
    assert!(!definition.definition_hash.is_empty());
    Ok(())
}

#[test]
fn runtime_dispatch_profile_selector_drops_codex_approval_for_non_codex_override() {
    let config = harness_core::config::HarnessConfig::default();
    let inherited_profile = RuntimeProfile::new("inherited-default", RuntimeKind::CodexJsonrpc);
    let mut policy = harness_core::config::workflow::RuntimeDispatchPolicy {
        runtime_kind: Some("codex_jsonrpc".to_string()),
        runtime_profile: Some("codex-default".to_string()),
        approval_policy: Some("on-request".to_string()),
        ..Default::default()
    };
    policy.workflow_profiles.insert(
        "claude_review".to_string(),
        harness_core::config::workflow::RuntimeDispatchProfileOverride {
            runtime_kind: Some("claude_code".to_string()),
            runtime_profile: Some("claude-review".to_string()),
            approval_policy: Some("never".to_string()),
            ..Default::default()
        },
    );
    policy.workflow_profiles.insert(
        "codex_review".to_string(),
        harness_core::config::workflow::RuntimeDispatchProfileOverride {
            runtime_profile: Some("codex-review".to_string()),
            ..Default::default()
        },
    );

    let selector = runtime_dispatch_profile_selector(&config, &policy, &inherited_profile).unwrap();
    let claude_profile = selector.select_for_workflow(Some("claude_review"));
    assert_eq!(claude_profile.kind, RuntimeKind::ClaudeCode);
    assert_eq!(claude_profile.name, "claude-review");
    assert_eq!(claude_profile.approval_policy, None);

    let codex_profile = selector.select_for_workflow(Some("codex_review"));
    assert_eq!(codex_profile.kind, RuntimeKind::CodexJsonrpc);
    assert_eq!(codex_profile.name, "codex-review");
    assert_eq!(codex_profile.approval_policy.as_deref(), Some("on-request"));
}

#[test]
fn runtime_dispatch_profile_drops_codex_approval_for_non_codex_default() {
    let config = harness_core::config::HarnessConfig::default();
    let inherited_profile = RuntimeProfile::new("inherited-default", RuntimeKind::CodexJsonrpc);
    let policy = harness_core::config::workflow::RuntimeDispatchPolicy {
        runtime_kind: Some("claude_code".to_string()),
        runtime_profile: Some("claude-default".to_string()),
        approval_policy: Some("on-request".to_string()),
        ..Default::default()
    };

    let profile = runtime_dispatch_profile(&config, &policy, &inherited_profile).unwrap();
    assert_eq!(profile.kind, RuntimeKind::ClaudeCode);
    assert_eq!(profile.name, "claude-default");
    assert_eq!(profile.approval_policy, None);
}

#[test]
fn runtime_worker_loop_policy_keeps_polling_when_server_root_disables_worker() {
    let policy = harness_core::config::workflow::RuntimeWorkerPolicy {
        enabled: false,
        concurrency: 8,
        ..Default::default()
    };

    let effective = runtime_worker_loop_policy(policy);

    assert!(effective.enabled);
    assert_eq!(effective.concurrency, 10);
    assert_eq!(effective.interval_secs, 5);
}

#[test]
fn runtime_worker_loop_policy_clamps_zero_concurrency() {
    let policy = harness_core::config::workflow::RuntimeWorkerPolicy {
        enabled: true,
        concurrency: 0,
        ..Default::default()
    };

    let effective = runtime_worker_loop_policy(policy);

    assert_eq!(effective.concurrency, 1);
}

#[test]
fn runtime_worker_open_slots_preserves_capacity_when_one_worker_hangs() {
    assert_eq!(runtime_worker_open_slots(1, 6), 5);
    assert_eq!(runtime_worker_open_slots(6, 6), 0);
    assert_eq!(runtime_worker_open_slots(0, 0), 0);
    assert_eq!(runtime_worker_open_slots(1, 10), 9);
}

#[test]
fn runtime_worker_state_owner_gate_ignores_worker_clones() {
    assert!(runtime_worker_has_external_state_owner(2, 0));
    assert!(runtime_worker_has_external_state_owner(4, 2));
    assert!(!runtime_worker_has_external_state_owner(1, 0));
    assert!(!runtime_worker_has_external_state_owner(3, 2));
}

#[tokio::test]
async fn drain_finished_runtime_worker_ticks_does_not_wait_for_hanging_worker() {
    let mut workers = tokio::task::JoinSet::new();
    workers.spawn(async {
        std::future::pending::<
                anyhow::Result<crate::workflow_runtime_worker::RuntimeJobWorkerTick>,
            >()
            .await
    });
    workers.spawn(async {
        Ok(crate::workflow_runtime_worker::RuntimeJobWorkerTick {
            succeeded: 1,
            ..Default::default()
        })
    });

    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        loop {
            drain_finished_runtime_worker_ticks(&mut workers);
            if workers.len() == 1 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("finished worker should be drained without waiting for hung worker");

    workers.abort_all();
    while workers.join_next().await.is_some() {}
}

#[tokio::test]
async fn runtime_worker_sleep_or_shutdown_exits_before_retry_delay() {
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::broadcast::channel(1);
    let mut workers = tokio::task::JoinSet::new();
    workers.spawn(async {
        std::future::pending::<
                anyhow::Result<crate::workflow_runtime_worker::RuntimeJobWorkerTick>,
            >()
            .await
    });
    shutdown_tx
        .send(())
        .expect("shutdown signal should be sent to subscribed receiver");

    let stopped = tokio::time::timeout(
        std::time::Duration::from_millis(100),
        runtime_worker_sleep_or_shutdown(
            std::time::Duration::from_secs(RUNTIME_WORKFLOW_CONFIG_RETRY_SECS),
            &mut shutdown_rx,
            &mut workers,
        ),
    )
    .await
    .expect("shutdown should interrupt retry delay");

    assert!(stopped);
    while workers.join_next().await.is_some() {}
}

// ARCH-GH-EXEMPT test double: mirrors the logic of fetch_pr_state_by_url in
// reconciliation.rs, but with an injectable gh_bin so tests run without a live
// GitHub connection. Keep the parsing (trim_matches('"'), to_uppercase) in sync
// with classify_pr_output in reconciliation.rs to avoid logic drift.
/// Fetch the current GitHub state for a PR identified by `pr_url`.
///
/// Returns `Some((raw_state, new_status))` only for actionable terminal states
/// (MERGED → Done, CLOSED → Cancelled). Any transient failure (I/O error,
/// non-zero exit, timeout, unexpected state) returns `None` so the caller
/// skips the task silently.
async fn fetch_pr_github_state(
    gh_bin: &str,
    task_id: &task_runner::TaskId,
    pr_url: &str,
) -> Option<(String, task_runner::TaskStatus)> {
    let output = match tokio::time::timeout(
        RECONCILE_GH_TIMEOUT,
        tokio::process::Command::new(gh_bin)
            .args(["pr", "view", pr_url, "--json", "state", "--jq", ".state"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .output(),
    )
    .await
    {
        Ok(Ok(out)) => out,
        Ok(Err(e)) => {
            tracing::warn!(task_id = %task_id, error = %e, "reconciliation: gh command failed");
            return None;
        }
        Err(_) => {
            tracing::warn!(task_id = %task_id, "reconciliation: gh command timed out");
            return None;
        }
    };
    if !output.status.success() {
        tracing::debug!(
            task_id = %task_id,
            stderr = %String::from_utf8_lossy(&output.stderr),
            "reconciliation: gh pr view returned non-zero"
        );
        return None;
    }
    let raw = String::from_utf8_lossy(&output.stdout)
        .trim()
        .trim_matches('"')
        .to_uppercase();
    match raw.as_str() {
        "MERGED" => Some((raw.to_string(), task_runner::TaskStatus::Done)),
        "CLOSED" => Some((raw.to_string(), task_runner::TaskStatus::Cancelled)),
        _ => None,
    }
}

#[test]
fn workflow_recovery_task_ids_only_uses_active_addressing_feedback_rows() {
    let mut addressing = IssueWorkflowInstance::new(
        "/tmp/project".to_string(),
        Some("owner/repo".to_string()),
        1,
    );
    addressing.state = IssueLifecycleState::AddressingFeedback;
    addressing.active_task_id = Some("task-1".to_string());

    let mut waiting = IssueWorkflowInstance::new(
        "/tmp/project".to_string(),
        Some("owner/repo".to_string()),
        2,
    );
    waiting.state = IssueLifecycleState::AwaitingFeedback;
    waiting.active_task_id = Some("task-2".to_string());

    let mut no_task = IssueWorkflowInstance::new(
        "/tmp/project".to_string(),
        Some("owner/repo".to_string()),
        3,
    );
    no_task.state = IssueLifecycleState::AddressingFeedback;

    let ids = workflow_recovery_task_ids(&[addressing, waiting, no_task]);
    assert_eq!(ids.len(), 1);
    assert!(ids.contains(&harness_core::types::TaskId("task-1".to_string())));
}

#[test]
fn workflow_recovery_task_ids_ignores_claim_placeholder_ids() {
    let mut claimed = IssueWorkflowInstance::new(
        "/tmp/project".to_string(),
        Some("owner/repo".to_string()),
        4,
    );
    claimed.state = IssueLifecycleState::AddressingFeedback;
    claimed.active_task_id = Some("claim:workflow-4".to_string());

    let ids = workflow_recovery_task_ids(&[claimed]);
    assert!(ids.is_empty());
}

#[test]
fn parse_issue_pr_uses_persisted_request_identifiers() {
    let issue_settings =
        task_runner::PersistedRequestSettings::from_req(&task_runner::CreateTaskRequest {
            issue: Some(944),
            prompt: Some("extra context".to_string()),
            external_id: Some("custom-issue-944".to_string()),
            ..task_runner::CreateTaskRequest::default()
        });
    let mut issue_task = task_runner::TaskState::new(task_runner::TaskId::new());
    issue_task.task_kind = task_runner::TaskKind::Issue;
    issue_task.external_id = Some("custom-issue-944".to_string());
    issue_task.request_settings = Some(issue_settings);
    assert_eq!(parse_issue_pr(&issue_task), (Some(944), None));

    let pr_settings =
        task_runner::PersistedRequestSettings::from_req(&task_runner::CreateTaskRequest {
            pr: Some(1040),
            external_id: Some("custom-pr-1040".to_string()),
            ..task_runner::CreateTaskRequest::default()
        });
    let mut pr_task = task_runner::TaskState::new(task_runner::TaskId::new());
    pr_task.task_kind = task_runner::TaskKind::Pr;
    pr_task.external_id = Some("custom-pr-1040".to_string());
    pr_task.request_settings = Some(pr_settings);
    assert_eq!(parse_issue_pr(&pr_task), (None, Some(1040)));
}

#[test]
fn prompt_orphan_recovery_requires_prompt_task_kind() {
    let settings =
        task_runner::PersistedRequestSettings::from_req(&task_runner::CreateTaskRequest {
            issue: Some(944),
            prompt: Some("extra context".to_string()),
            ..task_runner::CreateTaskRequest::default()
        });
    let mut issue_task = task_runner::TaskState::new(task_runner::TaskId::new());
    issue_task.task_kind = task_runner::TaskKind::Issue;
    issue_task.request_settings = Some(settings);
    assert!(!task_allows_prompt_orphan_recovery(&issue_task));

    let prompt_settings =
        task_runner::PersistedRequestSettings::from_req(&task_runner::CreateTaskRequest {
            prompt: Some("restart-safe prompt".to_string()),
            ..task_runner::CreateTaskRequest::default()
        });
    let mut prompt_task = task_runner::TaskState::new(task_runner::TaskId::new());
    prompt_task.task_kind = task_runner::TaskKind::Prompt;
    prompt_task.request_settings = Some(prompt_settings);
    assert!(task_allows_prompt_orphan_recovery(&prompt_task));
}

#[test]
fn task_is_pr_recovery_candidate_requires_pending_with_pr_url() {
    let mut pending_with_pr = task_runner::TaskState::new(task_runner::TaskId::new());
    pending_with_pr.status = task_runner::TaskStatus::Pending;
    pending_with_pr.pr_url = Some("https://github.com/owner/repo/pull/1".to_string());
    assert!(task_is_pr_recovery_candidate(&pending_with_pr));

    let mut waiting_with_pr = pending_with_pr.clone();
    waiting_with_pr.status = task_runner::TaskStatus::Waiting;
    assert!(!task_is_pr_recovery_candidate(&waiting_with_pr));

    let mut pending_without_pr = pending_with_pr;
    pending_without_pr.pr_url = None;
    assert!(!task_is_pr_recovery_candidate(&pending_without_pr));
}

#[test]
fn warn_dedup_insert_returns_true_first_time_only() {
    let mut warned: std::collections::HashSet<(String, Option<String>)> =
        std::collections::HashSet::new();
    let key = ("/dead/path".to_string(), Some("owner/repo".to_string()));
    assert!(
        warned.insert(key.clone()),
        "first insert should return true (first tick should warn)"
    );
    assert!(
        !warned.insert(key.clone()),
        "second insert should return false (dedup suppresses warn)"
    );
    warned.remove(&key);
    assert!(
        warned.insert(key.clone()),
        "after removal (path recovered), insert returns true again"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn reconcile_merged_transitions_to_done() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("gh");
    std::fs::write(&script, "#!/bin/sh\nprintf 'MERGED\\n'\n").unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let task_id = task_runner::TaskId::new();
    let result = fetch_pr_github_state(
        script.to_str().unwrap(),
        &task_id,
        "https://github.com/owner/repo/pull/1",
    )
    .await;
    assert!(
        matches!(result, Some((ref s, task_runner::TaskStatus::Done)) if s == "MERGED"),
        "expected Some((\"MERGED\", Done)), got {result:?}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn reconcile_closed_transitions_to_cancelled() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("gh");
    std::fs::write(&script, "#!/bin/sh\nprintf 'CLOSED\\n'\n").unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let task_id = task_runner::TaskId::new();
    let result = fetch_pr_github_state(
        script.to_str().unwrap(),
        &task_id,
        "https://github.com/owner/repo/pull/1",
    )
    .await;
    assert!(
        matches!(result, Some((ref s, task_runner::TaskStatus::Cancelled)) if s == "CLOSED"),
        "expected Some((\"CLOSED\", Cancelled)), got {result:?}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn reconcile_open_returns_none() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("gh");
    std::fs::write(&script, "#!/bin/sh\nprintf 'OPEN\\n'\n").unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let task_id = task_runner::TaskId::new();
    let result = fetch_pr_github_state(
        script.to_str().unwrap(),
        &task_id,
        "https://github.com/owner/repo/pull/1",
    )
    .await;
    assert!(
        result.is_none(),
        "expected None for OPEN state, got {result:?}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn reconcile_gh_failure_returns_none() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("gh");
    std::fs::write(&script, "#!/bin/sh\nexit 1\n").unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let task_id = task_runner::TaskId::new();
    let result = fetch_pr_github_state(
        script.to_str().unwrap(),
        &task_id,
        "https://github.com/owner/repo/pull/1",
    )
    .await;
    assert!(
        result.is_none(),
        "expected None for non-zero exit, got {result:?}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn reconcile_gh_timeout_returns_none() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("gh");
    std::fs::write(&script, "#!/bin/sh\nsleep 30\n").unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let task_id = task_runner::TaskId::new();
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(15),
        fetch_pr_github_state(
            script.to_str().unwrap(),
            &task_id,
            "https://github.com/owner/repo/pull/1",
        ),
    )
    .await;
    // The inner function must time out after RECONCILE_GH_TIMEOUT (10s) and return None;
    // the outer 15s guard is a safety net — if it fires, the inner timeout logic is broken.
    match result {
        Ok(inner) => assert!(inner.is_none(), "expected None on timeout, got {inner:?}"),
        Err(_) => panic!("outer test timeout fired before inner function timeout logic"),
    }
}
