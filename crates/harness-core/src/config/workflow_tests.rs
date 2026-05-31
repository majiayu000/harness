use super::*;

#[test]
fn load_workflow_config_defaults_when_missing() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let cfg = load_workflow_config(dir.path())?;
    assert_eq!(cfg.workflow.version, 1);
    assert_eq!(cfg.workflow.id, None);
    assert_eq!(cfg.source.kind, None);
    assert_eq!(cfg.source.repo, None);
    assert!(cfg.source.active_labels.is_empty());
    assert!(cfg.source.ignore_labels.is_empty());
    assert_eq!(cfg.base.remote, "origin");
    assert_eq!(cfg.base.branch, "main");
    assert!(cfg.base.require_remote_head);
    assert_eq!(cfg.workspace.strategy, "worktree");
    assert_eq!(cfg.workspace.root, None);
    assert_eq!(cfg.workspace.branch_prefix, "harness/");
    assert!(!cfg.workspace.reuse_existing_workspace);
    assert_eq!(cfg.workspace.cleanup, "on_terminal");
    assert_eq!(cfg.hooks.after_create, None);
    assert_eq!(cfg.hooks.before_run, None);
    assert_eq!(cfg.hooks.after_run, None);
    assert_eq!(cfg.hooks.before_remove, None);
    assert_eq!(cfg.hooks.timeout_secs, 60);
    assert_eq!(cfg.issue_workflow.force_execute_label, "force-execute");
    assert!(cfg.pr_feedback.enabled);
    assert_eq!(cfg.pr_feedback.sweep_interval_secs, 60);
    assert_eq!(cfg.pr_feedback.claim_stale_after_secs, 300);
    assert!(cfg.repo_backlog.enabled);
    assert_eq!(cfg.repo_backlog.poll_interval_secs, 60);
    assert_eq!(cfg.repo_backlog.batch_limit, 128);
    assert!(cfg.runtime_dispatch.enabled);
    assert_eq!(cfg.runtime_dispatch.interval_secs, 30);
    assert_eq!(cfg.runtime_dispatch.batch_limit, 25);
    assert_eq!(cfg.runtime_dispatch.runtime_kind, None);
    assert_eq!(cfg.runtime_dispatch.runtime_profile, None);
    assert_eq!(cfg.runtime_dispatch.model, None);
    assert_eq!(cfg.runtime_dispatch.reasoning_effort, None);
    assert_eq!(cfg.runtime_dispatch.sandbox, None);
    assert_eq!(cfg.runtime_dispatch.approval_policy, None);
    assert_eq!(cfg.runtime_dispatch.max_turns, None);
    assert_eq!(cfg.runtime_dispatch.timeout_secs, None);
    assert!(cfg.runtime_dispatch.workflow_profiles.is_empty());
    assert!(cfg.runtime_dispatch.activity_profiles.is_empty());
    assert!(cfg.runtime_dispatch.workflow_activity_profiles.is_empty());
    assert!(cfg.runtime_worker.enabled);
    assert_eq!(cfg.runtime_worker.interval_secs, 5);
    assert_eq!(cfg.runtime_worker.concurrency, 10);
    assert_eq!(cfg.runtime_worker.lease_ttl_secs, 3900);
    assert!(cfg.runtime_retry_policy.is_empty());
    assert!(cfg.issue_workflow.auto_replan_on_plan_issue);
    assert_eq!(cfg.storage.schema_namespace, "workflow");
    assert!(!cfg.issue_workflow.require_human_gate_before_merge);
    assert!(cfg.activities.is_empty());
    Ok(())
}

#[test]
fn load_workflow_config_keeps_runtime_dispatch_optional_fields_unset_when_omitted(
) -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    std::fs::write(
        dir.path().join("WORKFLOW.md"),
        r#"---
runtime_dispatch:
  enabled: true
  approval_policy: never
---

Body
"#,
    )?;

    let cfg = load_workflow_config(dir.path())?;
    assert_eq!(cfg.runtime_dispatch.runtime_kind, None);
    assert_eq!(cfg.runtime_dispatch.timeout_secs, None);
    assert_eq!(
        cfg.runtime_dispatch.approval_policy.as_deref(),
        Some("never")
    );
    Ok(())
}

#[test]
fn load_workflow_config_reads_front_matter() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    std::fs::write(
        dir.path().join("WORKFLOW.md"),
        r#"---
workflow:
  id: github_issue_pr
  version: 2
source:
  kind: github
  repo: owner/repo
  active_labels:
    - ready
  ignore_labels:
    - wontfix
base:
  remote: upstream
  branch: trunk
  require_remote_head: false
workspace:
  strategy: worktree
  root: /tmp/harness-workspaces
  branch_prefix: task/
  reuse_existing_workspace: true
  cleanup: never
hooks:
  after_create: echo after-create
  before_run: echo before-run
  after_run: echo after-run
  before_remove: echo before-remove
  timeout_secs: 9
issue_workflow:
  force_execute_label: do-not-second-guess
  auto_replan_on_plan_issue: false
  require_human_gate_before_merge: true
pr_feedback:
  enabled: false
  sweep_interval_secs: 15
  claim_stale_after_secs: 45
repo_backlog:
  enabled: false
  poll_interval_secs: 20
  batch_limit: 9
runtime_dispatch:
  enabled: true
  interval_secs: 5
  batch_limit: 7
  runtime_kind: codex_jsonrpc
  runtime_profile: codex-gpt-5.5-xhigh
  model: gpt-5.5
  reasoning_effort: xhigh
  sandbox: workspace-write
  approval_policy: on-request
  max_turns: 4
  timeout_secs: 600
  workflow_profiles:
    github_issue_pr:
      runtime_kind: codex_jsonrpc
      runtime_profile: codex-high
      model: gpt-5.4
      reasoning_effort: high
      max_turns: 8
    repo_backlog:
      runtime_profile: codex-backlog
      timeout_secs: 120
  activity_profiles:
    replan_issue:
      runtime_profile: codex-replan
      model: gpt-5.4-mini
      timeout_secs: 180
  workflow_activity_profiles:
    github_issue_pr:
      replan_issue:
        runtime_profile: codex-issue-replan
        model: gpt-5.4
        timeout_secs: 90
runtime_worker:
  enabled: true
  interval_secs: 3
  concurrency: 2
  lease_ttl_secs: 120
runtime_retry_policy:
  max_failed_activity_retries: 1
  retry_delay_secs: 30
  max_retry_delay_secs: 120
  activity_retries:
    implement_issue:
      max_failed_activity_retries: 2
      retry_delay_secs: 10
      max_retry_delay_secs: 60
storage:
  schema_namespace: orchestration
activities:
  implement_issue:
    prompt: issue-default
    validation:
      - cargo check
---

Body
"#,
    )?;

    let cfg = load_workflow_config(dir.path())?;
    assert_eq!(cfg.workflow.id.as_deref(), Some("github_issue_pr"));
    assert_eq!(cfg.workflow.version, 2);
    assert_eq!(cfg.source.kind.as_deref(), Some("github"));
    assert_eq!(cfg.source.repo.as_deref(), Some("owner/repo"));
    assert_eq!(cfg.source.active_labels, vec!["ready"]);
    assert_eq!(cfg.source.ignore_labels, vec!["wontfix"]);
    assert_eq!(cfg.base.remote, "upstream");
    assert_eq!(cfg.base.branch, "trunk");
    assert!(!cfg.base.require_remote_head);
    assert_eq!(cfg.workspace.strategy, "worktree");
    assert_eq!(
        cfg.workspace.root.as_deref(),
        Some("/tmp/harness-workspaces")
    );
    assert_eq!(cfg.workspace.branch_prefix, "task/");
    assert!(cfg.workspace.reuse_existing_workspace);
    assert_eq!(cfg.workspace.cleanup, "never");
    assert_eq!(cfg.hooks.after_create.as_deref(), Some("echo after-create"));
    assert_eq!(cfg.hooks.before_run.as_deref(), Some("echo before-run"));
    assert_eq!(cfg.hooks.after_run.as_deref(), Some("echo after-run"));
    assert_eq!(
        cfg.hooks.before_remove.as_deref(),
        Some("echo before-remove")
    );
    assert_eq!(cfg.hooks.timeout_secs, 9);
    assert_eq!(
        cfg.issue_workflow.force_execute_label,
        "do-not-second-guess"
    );
    assert!(!cfg.issue_workflow.auto_replan_on_plan_issue);
    assert!(cfg.issue_workflow.require_human_gate_before_merge);
    assert!(!cfg.pr_feedback.enabled);
    assert_eq!(cfg.pr_feedback.sweep_interval_secs, 15);
    assert_eq!(cfg.pr_feedback.claim_stale_after_secs, 45);
    assert!(!cfg.repo_backlog.enabled);
    assert_eq!(cfg.repo_backlog.poll_interval_secs, 20);
    assert_eq!(cfg.repo_backlog.batch_limit, 9);
    assert!(cfg.runtime_dispatch.enabled);
    assert_eq!(cfg.runtime_dispatch.interval_secs, 5);
    assert_eq!(cfg.runtime_dispatch.batch_limit, 7);
    assert_eq!(
        cfg.runtime_dispatch.runtime_kind.as_deref(),
        Some("codex_jsonrpc")
    );
    assert_eq!(
        cfg.runtime_dispatch.runtime_profile.as_deref(),
        Some("codex-gpt-5.5-xhigh")
    );
    assert_eq!(cfg.runtime_dispatch.model.as_deref(), Some("gpt-5.5"));
    assert_eq!(
        cfg.runtime_dispatch.reasoning_effort.as_deref(),
        Some("xhigh")
    );
    assert_eq!(
        cfg.runtime_dispatch.sandbox.as_deref(),
        Some("workspace-write")
    );
    assert_eq!(
        cfg.runtime_dispatch.approval_policy.as_deref(),
        Some("on-request")
    );
    assert_eq!(cfg.runtime_dispatch.max_turns, Some(4));
    assert_eq!(cfg.runtime_dispatch.timeout_secs, Some(600));
    let issue_profile = cfg
        .runtime_dispatch
        .workflow_profiles
        .get("github_issue_pr")
        .expect("issue workflow override should parse");
    assert_eq!(issue_profile.runtime_kind.as_deref(), Some("codex_jsonrpc"));
    assert_eq!(issue_profile.runtime_profile.as_deref(), Some("codex-high"));
    assert_eq!(issue_profile.model.as_deref(), Some("gpt-5.4"));
    assert_eq!(issue_profile.reasoning_effort.as_deref(), Some("high"));
    assert_eq!(issue_profile.max_turns, Some(8));
    let backlog_profile = cfg
        .runtime_dispatch
        .workflow_profiles
        .get("repo_backlog")
        .expect("repo backlog override should parse");
    assert_eq!(backlog_profile.runtime_kind, None);
    assert_eq!(
        backlog_profile.runtime_profile.as_deref(),
        Some("codex-backlog")
    );
    assert_eq!(backlog_profile.timeout_secs, Some(120));
    let replan_profile = cfg
        .runtime_dispatch
        .activity_profiles
        .get("replan_issue")
        .expect("activity profile override should parse");
    assert_eq!(
        replan_profile.runtime_profile.as_deref(),
        Some("codex-replan")
    );
    assert_eq!(replan_profile.model.as_deref(), Some("gpt-5.4-mini"));
    assert_eq!(replan_profile.timeout_secs, Some(180));
    let issue_replan_profile = cfg
        .runtime_dispatch
        .workflow_activity_profiles
        .get("github_issue_pr")
        .and_then(|profiles| profiles.get("replan_issue"))
        .expect("workflow activity profile override should parse");
    assert_eq!(
        issue_replan_profile.runtime_profile.as_deref(),
        Some("codex-issue-replan")
    );
    assert_eq!(issue_replan_profile.model.as_deref(), Some("gpt-5.4"));
    assert_eq!(issue_replan_profile.timeout_secs, Some(90));
    assert!(cfg.runtime_worker.enabled);
    assert_eq!(cfg.runtime_worker.interval_secs, 3);
    assert_eq!(cfg.runtime_worker.concurrency, 2);
    assert_eq!(cfg.runtime_worker.lease_ttl_secs, 120);
    assert_eq!(
        cfg.runtime_retry_policy.max_failed_activity_retries,
        Some(1)
    );
    assert_eq!(cfg.runtime_retry_policy.retry_delay_secs, Some(30));
    assert_eq!(cfg.runtime_retry_policy.max_retry_delay_secs, Some(120));
    assert_eq!(
        cfg.runtime_retry_policy
            .activity_retries
            .get("implement_issue")
            .and_then(|policy| policy.max_failed_activity_retries),
        Some(2)
    );
    assert_eq!(
        cfg.runtime_retry_policy
            .activity_retries
            .get("implement_issue")
            .and_then(|policy| policy.retry_delay_secs),
        Some(10)
    );
    assert_eq!(
        cfg.runtime_retry_policy
            .activity_retries
            .get("implement_issue")
            .and_then(|policy| policy.max_retry_delay_secs),
        Some(60)
    );
    assert_eq!(cfg.storage.schema_namespace, "orchestration");
    let activity = cfg
        .activities
        .get("implement_issue")
        .expect("activity should parse");
    assert_eq!(activity.prompt.as_deref(), Some("issue-default"));
    assert_eq!(activity.validation, vec!["cargo check"]);
    Ok(())
}

#[test]
fn load_workflow_document_reads_prompt_template_body() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    std::fs::write(
        dir.path().join("WORKFLOW.md"),
        "---\nworkflow:\n  id: prompt-test\n---\nUse issue {{ issue.number }}.\n",
    )?;

    let document = load_workflow_document(dir.path())?;
    assert_eq!(document.config.workflow.id.as_deref(), Some("prompt-test"));
    assert_eq!(document.prompt_template, "Use issue {{ issue.number }}.");
    assert!(document
        .source_path
        .as_deref()
        .is_some_and(|path| { path.ends_with("WORKFLOW.md") }));
    Ok(())
}

#[test]
fn load_workflow_document_uses_body_as_prompt_without_front_matter() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    std::fs::write(dir.path().join("WORKFLOW.md"), "Plain prompt\n")?;

    let document = load_workflow_document(dir.path())?;
    assert_eq!(document.config.base.branch, "main");
    assert_eq!(document.prompt_template, "Plain prompt");
    Ok(())
}

#[test]
fn load_workflow_config_reads_crlf_front_matter() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    std::fs::write(
        dir.path().join("WORKFLOW.md"),
        "---\r\nruntime_dispatch:\r\n  enabled: true\r\n  batch_limit: 9\r\n---\r\nBody\r\n",
    )?;

    let cfg = load_workflow_config(dir.path())?;
    assert!(cfg.runtime_dispatch.enabled);
    assert_eq!(cfg.runtime_dispatch.batch_limit, 9);
    Ok(())
}

#[test]
fn load_workflow_config_reads_front_matter_delimiter_at_eof() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    std::fs::write(
        dir.path().join("WORKFLOW.md"),
        "---\nruntime_worker:\n  enabled: true\n  concurrency: 3\n---",
    )?;

    let cfg = load_workflow_config(dir.path())?;
    assert!(cfg.runtime_worker.enabled);
    assert_eq!(cfg.runtime_worker.concurrency, 3);
    Ok(())
}

#[test]
fn load_workflow_config_reads_crlf_front_matter_delimiter_at_eof() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    std::fs::write(
        dir.path().join("WORKFLOW.md"),
        "---\r\nruntime_dispatch:\r\n  enabled: true\r\n  batch_limit: 11\r\n---",
    )?;

    let cfg = load_workflow_config(dir.path())?;
    assert!(cfg.runtime_dispatch.enabled);
    assert_eq!(cfg.runtime_dispatch.batch_limit, 11);
    Ok(())
}
