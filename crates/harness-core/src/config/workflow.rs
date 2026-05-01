use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

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
    pub issue_workflow: IssueWorkflowPolicy,
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

fn default_force_execute_label() -> String {
    "force-execute".to_string()
}

fn default_feedback_sweep_interval_secs() -> u64 {
    60
}

fn default_feedback_claim_stale_after_secs() -> u64 {
    300
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
    let path = project_root.join("WORKFLOW.md");
    let contents = match std::fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(WorkflowConfig::default()),
        Err(e) => return Err(e.into()),
    };

    let Some(front_matter) = extract_front_matter(&contents) else {
        return Ok(WorkflowConfig::default());
    };
    if front_matter.trim().is_empty() {
        return Ok(WorkflowConfig::default());
    }

    serde_yaml::from_str(front_matter).map_err(|e| {
        anyhow::anyhow!(
            "failed to parse workflow front matter at {}: {e}",
            path.display()
        )
    })
}

fn extract_front_matter(contents: &str) -> Option<&str> {
    let rest = contents
        .strip_prefix("---\r\n")
        .or_else(|| contents.strip_prefix("---\n"))?;

    let mut search_start = 0;
    while let Some(relative_idx) = rest[search_start..].find("---") {
        let idx = search_start + relative_idx;
        let at_line_start = idx == 0 || rest.as_bytes().get(idx - 1) == Some(&b'\n');
        let after = &rest[idx + 3..];
        let delimiter_ends_line =
            after.starts_with("\r\n") || after.starts_with('\n') || after.is_empty();
        if at_line_start && delimiter_ends_line {
            let front_matter = &rest[..idx];
            return Some(
                front_matter
                    .strip_suffix("\r\n")
                    .or_else(|| front_matter.strip_suffix('\n'))
                    .unwrap_or(front_matter),
            );
        }
        search_start = idx + 3;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_workflow_config_defaults_when_missing() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let cfg = load_workflow_config(dir.path())?;
        assert_eq!(cfg.issue_workflow.force_execute_label, "force-execute");
        assert!(cfg.pr_feedback.enabled);
        assert_eq!(cfg.pr_feedback.sweep_interval_secs, 60);
        assert_eq!(cfg.pr_feedback.claim_stale_after_secs, 300);
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
        Ok(())
    }

    #[test]
    fn load_workflow_config_reads_front_matter() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        std::fs::write(
            dir.path().join("WORKFLOW.md"),
            r#"---
issue_workflow:
  force_execute_label: do-not-second-guess
  auto_replan_on_plan_issue: false
  require_human_gate_before_merge: true
pr_feedback:
  enabled: false
  sweep_interval_secs: 15
  claim_stale_after_secs: 45
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
---

Body
"#,
        )?;

        let cfg = load_workflow_config(dir.path())?;
        assert_eq!(
            cfg.issue_workflow.force_execute_label,
            "do-not-second-guess"
        );
        assert!(!cfg.issue_workflow.auto_replan_on_plan_issue);
        assert!(cfg.issue_workflow.require_human_gate_before_merge);
        assert!(!cfg.pr_feedback.enabled);
        assert_eq!(cfg.pr_feedback.sweep_interval_secs, 15);
        assert_eq!(cfg.pr_feedback.claim_stale_after_secs, 45);
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
