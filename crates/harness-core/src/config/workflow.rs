use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WorkflowDocument {
    #[serde(default)]
    pub config: WorkflowConfig,
    #[serde(default)]
    pub prompt_template: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_path: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowIdentityPolicy {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default = "default_workflow_version")]
    pub version: u32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WorkflowSourcePolicy {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    #[serde(default)]
    pub active_labels: Vec<String>,
    #[serde(default)]
    pub ignore_labels: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowBasePolicy {
    #[serde(default = "default_base_remote")]
    pub remote: String,
    #[serde(default = "default_base_branch")]
    pub branch: String,
    #[serde(default = "default_true")]
    pub require_remote_head: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowWorkspacePolicy {
    #[serde(default = "default_workspace_strategy")]
    pub strategy: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root: Option<String>,
    #[serde(default = "default_workspace_branch_prefix")]
    pub branch_prefix: String,
    #[serde(default)]
    pub reuse_existing_workspace: bool,
    #[serde(default = "default_workspace_cleanup")]
    pub cleanup: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowHooksPolicy {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after_create: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before_run: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after_run: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before_remove: Option<String>,
    #[serde(default = "default_hook_timeout_secs")]
    pub timeout_secs: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WorkflowActivityPolicy {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    #[serde(default)]
    pub validation: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IssueWorkflowPolicy {
    #[serde(default = "default_force_execute_label")]
    pub force_execute_label: String,
    #[serde(default = "default_true")]
    pub auto_replan_on_plan_issue: bool,
    /// When true, the review loop pauses at `ready_to_merge` and requires a
    /// human to call `POST /tasks/:id/merge` before the workflow advances to
    /// `done`.  Defaults to `false` to preserve the legacy auto-merge flow.
    #[serde(default)]
    pub require_human_gate_before_merge: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrFeedbackPolicy {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_feedback_sweep_interval_secs")]
    pub sweep_interval_secs: u64,
    #[serde(default = "default_feedback_claim_stale_after_secs")]
    pub claim_stale_after_secs: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoBacklogPolicy {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_repo_backlog_poll_interval_secs")]
    pub poll_interval_secs: u64,
    #[serde(default = "default_repo_backlog_batch_limit")]
    pub batch_limit: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeDispatchPolicy {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_runtime_dispatch_interval_secs")]
    pub interval_secs: u64,
    #[serde(default = "default_runtime_dispatch_batch_limit")]
    pub batch_limit: u32,
    #[serde(default = "default_runtime_dispatch_kind")]
    pub runtime_kind: String,
    #[serde(default = "default_runtime_dispatch_profile")]
    pub runtime_profile: String,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub reasoning_effort: Option<String>,
    #[serde(default)]
    pub sandbox: Option<String>,
    #[serde(default)]
    pub approval_policy: Option<String>,
    #[serde(default)]
    pub max_turns: Option<u32>,
    #[serde(default)]
    pub timeout_secs: Option<u64>,
    #[serde(default)]
    pub workflow_profiles: BTreeMap<String, RuntimeDispatchProfileOverride>,
    #[serde(default)]
    pub activity_profiles: BTreeMap<String, RuntimeDispatchProfileOverride>,
    #[serde(default)]
    pub workflow_activity_profiles:
        BTreeMap<String, BTreeMap<String, RuntimeDispatchProfileOverride>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RuntimeDispatchProfileOverride {
    #[serde(default)]
    pub runtime_kind: Option<String>,
    #[serde(default)]
    pub runtime_profile: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub reasoning_effort: Option<String>,
    #[serde(default)]
    pub sandbox: Option<String>,
    #[serde(default)]
    pub approval_policy: Option<String>,
    #[serde(default)]
    pub max_turns: Option<u32>,
    #[serde(default)]
    pub timeout_secs: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeWorkerPolicy {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_runtime_worker_interval_secs")]
    pub interval_secs: u64,
    #[serde(default = "default_runtime_worker_concurrency")]
    pub concurrency: u32,
    #[serde(default = "default_runtime_worker_lease_ttl_secs")]
    pub lease_ttl_secs: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RuntimeRetryPolicy {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_failed_activity_retries: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_delay_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_retry_delay_secs: Option<u64>,
    #[serde(default)]
    pub activity_retries: BTreeMap<String, RuntimeActivityRetryPolicy>,
}

impl RuntimeRetryPolicy {
    pub fn is_empty(&self) -> bool {
        self.max_failed_activity_retries.is_none()
            && self.retry_delay_secs.is_none()
            && self.max_retry_delay_secs.is_none()
            && self.activity_retries.is_empty()
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RuntimeActivityRetryPolicy {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_failed_activity_retries: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_delay_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_retry_delay_secs: Option<u64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WorkflowConfig {
    #[serde(default)]
    pub workflow: WorkflowIdentityPolicy,
    #[serde(default)]
    pub source: WorkflowSourcePolicy,
    #[serde(default)]
    pub base: WorkflowBasePolicy,
    #[serde(default)]
    pub workspace: WorkflowWorkspacePolicy,
    #[serde(default)]
    pub hooks: WorkflowHooksPolicy,
    #[serde(default)]
    pub issue_workflow: IssueWorkflowPolicy,
    #[serde(default)]
    pub repo_backlog: RepoBacklogPolicy,
    #[serde(default)]
    pub pr_feedback: PrFeedbackPolicy,
    #[serde(default)]
    pub runtime_dispatch: RuntimeDispatchPolicy,
    #[serde(default)]
    pub runtime_worker: RuntimeWorkerPolicy,
    #[serde(default)]
    pub runtime_retry_policy: RuntimeRetryPolicy,
    #[serde(default)]
    pub storage: WorkflowStoragePolicy,
    #[serde(default)]
    pub activities: BTreeMap<String, WorkflowActivityPolicy>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowStoragePolicy {
    #[serde(default = "default_workflow_schema_namespace")]
    pub schema_namespace: String,
}

impl Default for IssueWorkflowPolicy {
    fn default() -> Self {
        Self {
            force_execute_label: default_force_execute_label(),
            auto_replan_on_plan_issue: default_true(),
            require_human_gate_before_merge: false,
        }
    }
}

impl Default for PrFeedbackPolicy {
    fn default() -> Self {
        Self {
            enabled: default_true(),
            sweep_interval_secs: default_feedback_sweep_interval_secs(),
            claim_stale_after_secs: default_feedback_claim_stale_after_secs(),
        }
    }
}

impl Default for RepoBacklogPolicy {
    fn default() -> Self {
        Self {
            enabled: default_true(),
            poll_interval_secs: default_repo_backlog_poll_interval_secs(),
            batch_limit: default_repo_backlog_batch_limit(),
        }
    }
}

impl Default for RuntimeDispatchPolicy {
    fn default() -> Self {
        Self {
            enabled: true,
            interval_secs: default_runtime_dispatch_interval_secs(),
            batch_limit: default_runtime_dispatch_batch_limit(),
            runtime_kind: default_runtime_dispatch_kind(),
            runtime_profile: default_runtime_dispatch_profile(),
            model: None,
            reasoning_effort: None,
            sandbox: None,
            approval_policy: None,
            max_turns: None,
            timeout_secs: None,
            workflow_profiles: BTreeMap::new(),
            activity_profiles: BTreeMap::new(),
            workflow_activity_profiles: BTreeMap::new(),
        }
    }
}

impl Default for RuntimeWorkerPolicy {
    fn default() -> Self {
        Self {
            enabled: true,
            interval_secs: default_runtime_worker_interval_secs(),
            concurrency: default_runtime_worker_concurrency(),
            lease_ttl_secs: default_runtime_worker_lease_ttl_secs(),
        }
    }
}

impl Default for WorkflowStoragePolicy {
    fn default() -> Self {
        Self {
            schema_namespace: default_workflow_schema_namespace(),
        }
    }
}

impl Default for WorkflowIdentityPolicy {
    fn default() -> Self {
        Self {
            id: None,
            version: default_workflow_version(),
        }
    }
}

impl Default for WorkflowBasePolicy {
    fn default() -> Self {
        Self {
            remote: default_base_remote(),
            branch: default_base_branch(),
            require_remote_head: true,
        }
    }
}

impl Default for WorkflowWorkspacePolicy {
    fn default() -> Self {
        Self {
            strategy: default_workspace_strategy(),
            root: None,
            branch_prefix: default_workspace_branch_prefix(),
            reuse_existing_workspace: false,
            cleanup: default_workspace_cleanup(),
        }
    }
}

impl Default for WorkflowHooksPolicy {
    fn default() -> Self {
        Self {
            after_create: None,
            before_run: None,
            after_run: None,
            before_remove: None,
            timeout_secs: default_hook_timeout_secs(),
        }
    }
}

fn default_workflow_version() -> u32 {
    1
}

fn default_base_remote() -> String {
    "origin".to_string()
}

fn default_base_branch() -> String {
    "main".to_string()
}

fn default_workspace_strategy() -> String {
    "worktree".to_string()
}

fn default_workspace_branch_prefix() -> String {
    "harness/".to_string()
}

fn default_workspace_cleanup() -> String {
    "on_terminal".to_string()
}

fn default_hook_timeout_secs() -> u64 {
    60
}

fn default_force_execute_label() -> String {
    "force-execute".to_string()
}

fn default_feedback_sweep_interval_secs() -> u64 {
    60
}

fn default_feedback_claim_stale_after_secs() -> u64 {
    300
}

fn default_repo_backlog_poll_interval_secs() -> u64 {
    60
}

fn default_repo_backlog_batch_limit() -> u32 {
    128
}

fn default_runtime_dispatch_interval_secs() -> u64 {
    30
}

fn default_runtime_dispatch_batch_limit() -> u32 {
    25
}

fn default_runtime_dispatch_kind() -> String {
    "codex_jsonrpc".to_string()
}

fn default_runtime_dispatch_profile() -> String {
    "codex-default".to_string()
}

fn default_runtime_worker_interval_secs() -> u64 {
    5
}

fn default_runtime_worker_concurrency() -> u32 {
    1
}

fn default_runtime_worker_lease_ttl_secs() -> u64 {
    900
}

fn default_workflow_schema_namespace() -> String {
    "workflow".to_string()
}

fn default_true() -> bool {
    true
}

/// Load workflow policy from `{project_root}/WORKFLOW.md`.
///
/// Only the YAML front matter is parsed. Missing files or missing front matter
/// fall back to defaults.
pub fn load_workflow_config(project_root: &Path) -> anyhow::Result<WorkflowConfig> {
    load_workflow_document(project_root).map(|document| document.config)
}

/// Load the workflow policy and prompt template from `{project_root}/WORKFLOW.md`.
///
/// Missing files fall back to defaults with an empty prompt template. Files
/// without front matter use default config and treat the whole body as the
/// repository workflow prompt template.
pub fn load_workflow_document(project_root: &Path) -> anyhow::Result<WorkflowDocument> {
    let path = project_root.join("WORKFLOW.md");
    let contents = match std::fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(WorkflowDocument::default());
        }
        Err(e) => return Err(e.into()),
    };

    let (front_matter, prompt_template) = split_front_matter_and_body(&contents);
    let config = match front_matter {
        None => WorkflowConfig::default(),
        Some(front_matter) if front_matter.trim().is_empty() => WorkflowConfig::default(),
        Some(front_matter) => serde_yaml::from_str(front_matter).map_err(|e| {
            anyhow::anyhow!(
                "failed to parse workflow front matter at {}: {e}",
                path.display()
            )
        })?,
    };

    Ok(WorkflowDocument {
        config,
        prompt_template: prompt_template.trim().to_string(),
        source_path: Some(path.display().to_string()),
    })
}

fn split_front_matter_and_body(contents: &str) -> (Option<&str>, &str) {
    let rest = contents
        .strip_prefix("---\r\n")
        .or_else(|| contents.strip_prefix("---\n"));
    let Some(rest) = rest else {
        return (None, contents);
    };

    let mut search_start = 0;
    while let Some(relative_idx) = rest[search_start..].find("---") {
        let idx = search_start + relative_idx;
        let at_line_start = idx == 0 || rest.as_bytes().get(idx - 1) == Some(&b'\n');
        let after = &rest[idx + 3..];
        let delimiter_ends_line =
            after.starts_with("\r\n") || after.starts_with('\n') || after.is_empty();
        if at_line_start && delimiter_ends_line {
            let front_matter = &rest[..idx];
            let front_matter = front_matter
                .strip_suffix("\r\n")
                .or_else(|| front_matter.strip_suffix('\n'))
                .unwrap_or(front_matter);
            let body = after
                .strip_prefix("\r\n")
                .or_else(|| after.strip_prefix('\n'))
                .unwrap_or(after);
            return (Some(front_matter), body);
        }
        search_start = idx + 3;
    }
    (None, contents)
}

#[cfg(test)]
mod tests {
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
        assert_eq!(cfg.runtime_dispatch.runtime_kind, "codex_jsonrpc");
        assert_eq!(cfg.runtime_dispatch.runtime_profile, "codex-default");
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
        assert_eq!(cfg.runtime_worker.concurrency, 1);
        assert_eq!(cfg.runtime_worker.lease_ttl_secs, 900);
        assert!(cfg.runtime_retry_policy.is_empty());
        assert!(cfg.issue_workflow.auto_replan_on_plan_issue);
        assert_eq!(cfg.storage.schema_namespace, "workflow");
        assert!(!cfg.issue_workflow.require_human_gate_before_merge);
        assert!(cfg.activities.is_empty());
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
  runtime_kind: claude_code
  runtime_profile: claude-default
  model: claude-sonnet-4-6
  reasoning_effort: medium
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
        assert_eq!(cfg.runtime_dispatch.runtime_kind, "claude_code");
        assert_eq!(cfg.runtime_dispatch.runtime_profile, "claude-default");
        assert_eq!(
            cfg.runtime_dispatch.model.as_deref(),
            Some("claude-sonnet-4-6")
        );
        assert_eq!(
            cfg.runtime_dispatch.reasoning_effort.as_deref(),
            Some("medium")
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
}
