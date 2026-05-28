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
    #[serde(default = "default_runtime_dispatch_runtime_kind")]
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
    #[serde(default = "default_runtime_dispatch_timeout_secs")]
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

#[derive(Debug, Clone, Serialize, Deserialize)]
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
    #[serde(default = "default_workflow_runtime_dispatch_policy")]
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
            runtime_kind: None,
            runtime_profile: None,
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

impl Default for WorkflowConfig {
    fn default() -> Self {
        Self {
            workflow: WorkflowIdentityPolicy::default(),
            source: WorkflowSourcePolicy::default(),
            base: WorkflowBasePolicy::default(),
            workspace: WorkflowWorkspacePolicy::default(),
            hooks: WorkflowHooksPolicy::default(),
            issue_workflow: IssueWorkflowPolicy::default(),
            repo_backlog: RepoBacklogPolicy::default(),
            pr_feedback: PrFeedbackPolicy::default(),
            runtime_dispatch: default_workflow_runtime_dispatch_policy(),
            runtime_worker: RuntimeWorkerPolicy::default(),
            runtime_retry_policy: RuntimeRetryPolicy::default(),
            storage: WorkflowStoragePolicy::default(),
            activities: BTreeMap::new(),
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

fn default_runtime_dispatch_runtime_kind() -> Option<String> {
    Some("codex_exec".to_string())
}

fn default_runtime_dispatch_timeout_secs() -> Option<u64> {
    Some(3600)
}

fn default_workflow_runtime_dispatch_policy() -> RuntimeDispatchPolicy {
    RuntimeDispatchPolicy {
        runtime_kind: default_runtime_dispatch_runtime_kind(),
        timeout_secs: default_runtime_dispatch_timeout_secs(),
        ..RuntimeDispatchPolicy::default()
    }
}

fn default_runtime_worker_interval_secs() -> u64 {
    5
}

fn default_runtime_worker_concurrency() -> u32 {
    10
}

fn default_runtime_worker_lease_ttl_secs() -> u64 {
    3900
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
#[path = "workflow_tests.rs"]
mod tests;
