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
    assert!(cfg.pr_feedback.hygiene_enabled);
    assert_eq!(cfg.pr_feedback.hygiene_interval_secs, 1800);
    assert_eq!(cfg.pr_feedback.dirty_age_to_repair_secs, 172800);
    assert_eq!(cfg.pr_feedback.dirty_age_to_comment_secs, 604800);
    assert_eq!(cfg.pr_feedback.rebase_needed_label, "rebase-needed");
    assert_eq!(cfg.pr_feedback.hygiene_batch_limit, 25);
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
    assert!(!cfg.memory.enabled);
    assert!(!cfg.candidates.enabled);
    assert_eq!(cfg.candidates.n, 2);
    assert_eq!(cfg.candidates.trigger_label, "best-of-n");
    assert_eq!(cfg.candidates.max_turns_per_candidate, None);
    assert!(cfg.issue_workflow.auto_replan_on_plan_issue);
    assert!(cfg.pr_scope_guard.enabled);
    assert_eq!(cfg.pr_scope_guard.max_files_changed, 30);
    assert_eq!(cfg.pr_scope_guard.max_lines_added, 1500);
    assert_eq!(cfg.storage.schema_namespace, "workflow");
    assert!(cfg.storage.orphan_reaper_enabled);
    assert_eq!(cfg.storage.orphan_reaper_interval_secs, 3600);
    assert!(cfg.storage.orphan_reaper_legacy_enabled);
    assert_eq!(cfg.storage.orphan_reaper_legacy_batch, 200);
    assert!(!cfg.storage.workflow_watchdog_enabled);
    assert_eq!(cfg.storage.workflow_watchdog_age_minutes, 240);
    assert_eq!(cfg.storage.workflow_watchdog_interval_secs, 300);
    assert_eq!(cfg.storage.workflow_watchdog_batch_size, 100);
    assert!(!cfg.storage.runtime_retention_enabled);
    assert_eq!(cfg.storage.runtime_retention_days, 30);
    assert_eq!(cfg.storage.runtime_retention_batch_size, 1000);
    assert_eq!(cfg.storage.runtime_retention_interval_secs, 3600);
    assert!(!cfg.storage.task_retention_enabled);
    assert_eq!(cfg.storage.task_retention_days, 30);
    assert_eq!(cfg.storage.task_retention_batch_size, 1000);
    assert_eq!(cfg.storage.task_retention_interval_secs, 3600);
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
pr_scope_guard:
  enabled: false
  max_files_changed: 12
  max_lines_added: 345
pr_feedback:
  enabled: false
  sweep_interval_secs: 15
  claim_stale_after_secs: 45
  hygiene_enabled: false
  hygiene_interval_secs: 1801
  dirty_age_to_repair_secs: 1802
  dirty_age_to_comment_secs: 1803
  rebase_needed_label: needs-rebase
  hygiene_batch_limit: 6
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
memory:
  enabled: true
candidates:
  enabled: true
  n: 3
  trigger_label: frontier
  max_turns_per_candidate: 4
storage:
  schema_namespace: orchestration
  orphan_reaper_enabled: false
  orphan_reaper_interval_secs: 44
  orphan_reaper_legacy_enabled: false
  orphan_reaper_legacy_batch: 55
  workflow_watchdog_enabled: true
  workflow_watchdog_age_minutes: 12
  workflow_watchdog_interval_secs: 13
  workflow_watchdog_batch_size: 14
  runtime_retention_enabled: true
  runtime_retention_days: 45
  runtime_retention_batch_size: 46
  runtime_retention_interval_secs: 47
  task_retention_enabled: true
  task_retention_days: 48
  task_retention_batch_size: 49
  task_retention_interval_secs: 50
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
    assert!(!cfg.pr_scope_guard.enabled);
    assert_eq!(cfg.pr_scope_guard.max_files_changed, 12);
    assert_eq!(cfg.pr_scope_guard.max_lines_added, 345);
    assert!(!cfg.pr_feedback.enabled);
    assert_eq!(cfg.pr_feedback.sweep_interval_secs, 15);
    assert_eq!(cfg.pr_feedback.claim_stale_after_secs, 45);
    assert!(!cfg.pr_feedback.hygiene_enabled);
    assert_eq!(cfg.pr_feedback.hygiene_interval_secs, 1801);
    assert_eq!(cfg.pr_feedback.dirty_age_to_repair_secs, 1802);
    assert_eq!(cfg.pr_feedback.dirty_age_to_comment_secs, 1803);
    assert_eq!(cfg.pr_feedback.rebase_needed_label, "needs-rebase");
    assert_eq!(cfg.pr_feedback.hygiene_batch_limit, 6);
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
    assert!(cfg.memory.enabled);
    assert!(cfg.candidates.enabled);
    assert_eq!(cfg.candidates.n, 3);
    assert_eq!(cfg.candidates.trigger_label, "frontier");
    assert_eq!(cfg.candidates.max_turns_per_candidate, Some(4));
    assert_eq!(cfg.storage.schema_namespace, "orchestration");
    assert!(!cfg.storage.orphan_reaper_enabled);
    assert_eq!(cfg.storage.orphan_reaper_interval_secs, 44);
    assert!(!cfg.storage.orphan_reaper_legacy_enabled);
    assert_eq!(cfg.storage.orphan_reaper_legacy_batch, 55);
    assert!(cfg.storage.workflow_watchdog_enabled);
    assert_eq!(cfg.storage.workflow_watchdog_age_minutes, 12);
    assert_eq!(cfg.storage.workflow_watchdog_interval_secs, 13);
    assert_eq!(cfg.storage.workflow_watchdog_batch_size, 14);
    assert!(cfg.storage.runtime_retention_enabled);
    assert_eq!(cfg.storage.runtime_retention_days, 45);
    assert_eq!(cfg.storage.runtime_retention_batch_size, 46);
    assert_eq!(cfg.storage.runtime_retention_interval_secs, 47);
    assert!(cfg.storage.task_retention_enabled);
    assert_eq!(cfg.storage.task_retention_days, 48);
    assert_eq!(cfg.storage.task_retention_batch_size, 49);
    assert_eq!(cfg.storage.task_retention_interval_secs, 50);
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
fn load_workflow_config_reads_custom_activity_profile_overrides() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    std::fs::write(
        dir.path().join("WORKFLOW.md"),
        r#"---
runtime_dispatch:
  activity_profiles:
    inspect_pr:
      runtime_kind: codex_exec
      runtime_profile: codex-pr-inspector
      timeout_secs: 7200
  workflow_activity_profiles:
    github_issue_pr:
      inspect_pr:
        timeout_secs: 5400
---
Body
"#,
    )?;

    let cfg = load_workflow_config(dir.path())?;
    let inspect_profile = cfg.runtime_dispatch.activity_profiles.get("inspect_pr");
    assert_eq!(
        inspect_profile.and_then(|profile| profile.runtime_kind.as_deref()),
        Some("codex_exec")
    );
    assert_eq!(
        inspect_profile.and_then(|profile| profile.runtime_profile.as_deref()),
        Some("codex-pr-inspector")
    );
    assert_eq!(
        inspect_profile.and_then(|profile| profile.timeout_secs),
        Some(7200)
    );
    assert_eq!(
        cfg.runtime_dispatch
            .workflow_activity_profiles
            .get("github_issue_pr")
            .and_then(|profiles| profiles.get("inspect_pr"))
            .and_then(|profile| profile.timeout_secs),
        Some(5400)
    );
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

#[test]
fn deep_merge_yaml_overrides_only_declared_fields() {
    let base: serde_yaml::Value = serde_yaml::from_str(
        "runtime_retry_policy:\n  max_failed_activity_retries: 6\n  retry_delay_secs: 30\nactivity_profiles:\n  implement_issue:\n    prompt: base\n",
    )
    .unwrap();
    let over: serde_yaml::Value = serde_yaml::from_str(
        "runtime_retry_policy:\n  max_failed_activity_retries: 2\nactivity_profiles:\n  implement_issue:\n    validation:\n      - cargo test\n",
    )
    .unwrap();

    let merged = deep_merge_yaml(base, over);

    // The override replaces only the nested fields it declares; sibling base
    // fields survive (field-level override).
    assert_eq!(
        merged["runtime_retry_policy"]["max_failed_activity_retries"].as_u64(),
        Some(2)
    );
    assert_eq!(
        merged["runtime_retry_policy"]["retry_delay_secs"].as_u64(),
        Some(30)
    );
    assert_eq!(
        merged["activity_profiles"]["implement_issue"]["prompt"].as_str(),
        Some("base")
    );
    assert_eq!(
        merged["activity_profiles"]["implement_issue"]["validation"][0].as_str(),
        Some("cargo test")
    );
}

#[test]
fn deep_merge_yaml_null_override_keeps_base() {
    let base: serde_yaml::Value = serde_yaml::from_str("a: 1\n").unwrap();
    let over: serde_yaml::Value = serde_yaml::from_str("a: ~\n").unwrap();
    let merged = deep_merge_yaml(base, over);
    assert_eq!(merged["a"].as_u64(), Some(1));
}

#[test]
fn load_workflow_document_merges_base_then_repo_override() -> anyhow::Result<()> {
    let base_dir = tempfile::tempdir()?;
    let base_path = base_dir.path().join("WORKFLOW.md");
    std::fs::write(
        &base_path,
        "---\nruntime_worker:\n  concurrency: 4\nruntime_retry_policy:\n  max_failed_activity_retries: 6\n  retry_delay_secs: 30\n---\nbase body\n",
    )?;

    let repo_dir = tempfile::tempdir()?;
    std::fs::write(
        repo_dir.path().join("WORKFLOW.md"),
        "---\nruntime_worker:\n  interval_secs: 9\n---\nrepo body\n",
    )?;

    let doc = load_workflow_document_with_base(repo_dir.path(), Some(&base_path))?;

    // Repo overrides the field it declares while base sibling fields survive.
    assert_eq!(doc.config.runtime_worker.interval_secs, 9);
    assert_eq!(doc.config.runtime_worker.concurrency, 4);
    assert_eq!(
        doc.config.runtime_retry_policy.max_failed_activity_retries,
        Some(6)
    );
    assert_eq!(doc.config.runtime_retry_policy.retry_delay_secs, Some(30));
    // The repo body wins when present.
    assert_eq!(doc.prompt_template, "repo body");
    Ok(())
}

#[test]
fn load_workflow_document_inherits_base_when_repo_absent() -> anyhow::Result<()> {
    let base_dir = tempfile::tempdir()?;
    let base_path = base_dir.path().join("WORKFLOW.md");
    std::fs::write(
        &base_path,
        "---\nruntime_retry_policy:\n  max_failed_activity_retries: 5\n---\nbase body\n",
    )?;
    let repo_dir = tempfile::tempdir()?; // no WORKFLOW.md

    let doc = load_workflow_document_with_base(repo_dir.path(), Some(&base_path))?;

    assert_eq!(
        doc.config.runtime_retry_policy.max_failed_activity_retries,
        Some(5)
    );
    assert_eq!(doc.prompt_template, "base body");
    Ok(())
}

#[test]
fn workflow_path_identity_treats_canonical_and_relative_same_file_as_equal() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let repo_path = dir.path().join(".").join("WORKFLOW.md");
    std::fs::write(&repo_path, "---\nruntime_worker:\n  enabled: false\n---\n")?;
    let base_path = std::fs::canonicalize(dir.path().join("WORKFLOW.md"))?;

    assert!(!workflow_paths_are_distinct(&base_path, &repo_path));
    Ok(())
}

#[test]
fn load_workflow_document_without_base_uses_repo_only() -> anyhow::Result<()> {
    let repo_dir = tempfile::tempdir()?;
    std::fs::write(
        repo_dir.path().join("WORKFLOW.md"),
        "---\nruntime_retry_policy:\n  max_failed_activity_retries: 3\n---\n",
    )?;
    let doc = load_workflow_document_with_base(repo_dir.path(), None)?;
    assert_eq!(
        doc.config.runtime_retry_policy.max_failed_activity_retries,
        Some(3)
    );
    Ok(())
}
