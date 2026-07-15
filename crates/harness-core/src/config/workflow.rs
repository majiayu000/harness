use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

mod candidates;
mod storage;
pub use candidates::WorkflowCandidatesPolicy;
pub use storage::WorkflowStoragePolicy;

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
pub struct PrScopeGuardPolicy {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_pr_scope_guard_max_files_changed")]
    pub max_files_changed: u32,
    #[serde(default = "default_pr_scope_guard_max_lines_added")]
    pub max_lines_added: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrFeedbackPolicy {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_feedback_sweep_interval_secs")]
    pub sweep_interval_secs: u64,
    #[serde(default = "default_feedback_claim_stale_after_secs")]
    pub claim_stale_after_secs: u64,
    #[serde(default = "default_true")]
    pub hygiene_enabled: bool,
    #[serde(default = "default_pr_hygiene_interval_secs")]
    pub hygiene_interval_secs: u64,
    #[serde(default = "default_pr_hygiene_dirty_age_to_repair_secs")]
    pub dirty_age_to_repair_secs: u64,
    #[serde(default = "default_pr_hygiene_dirty_age_to_comment_secs")]
    pub dirty_age_to_comment_secs: u64,
    #[serde(default = "default_pr_hygiene_rebase_needed_label")]
    pub rebase_needed_label: String,
    #[serde(default = "default_pr_hygiene_batch_limit")]
    pub hygiene_batch_limit: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeDispatchPolicy {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_runtime_dispatch_interval_secs")]
    pub interval_secs: u64,
    #[serde(default = "default_runtime_dispatch_batch_limit")]
    pub batch_limit: u32,
    #[serde(default = "default_runtime_dispatch_defer_backoff_secs")]
    pub defer_backoff_secs: u64,
    #[serde(default = "default_runtime_dispatch_defer_backoff_max_secs")]
    pub defer_backoff_max_secs: u64,
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
    #[serde(default)]
    pub workflow_profiles: BTreeMap<String, RuntimeDispatchProfileOverride>,
    #[serde(default = "default_runtime_dispatch_activity_profiles")]
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
pub struct WorkflowMemoryPolicy {
    #[serde(default)]
    pub enabled: bool,
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
    pub pr_scope_guard: PrScopeGuardPolicy,
    #[serde(default)]
    pub pr_feedback: PrFeedbackPolicy,
    #[serde(default)]
    pub runtime_dispatch: RuntimeDispatchPolicy,
    #[serde(default)]
    pub runtime_worker: RuntimeWorkerPolicy,
    #[serde(default)]
    pub runtime_retry_policy: RuntimeRetryPolicy,
    #[serde(default)]
    pub memory: WorkflowMemoryPolicy,
    #[serde(default)]
    pub candidates: WorkflowCandidatesPolicy,
    #[serde(default)]
    pub storage: WorkflowStoragePolicy,
    #[serde(default)]
    pub activities: BTreeMap<String, WorkflowActivityPolicy>,
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

impl Default for PrScopeGuardPolicy {
    fn default() -> Self {
        Self {
            enabled: true,
            max_files_changed: default_pr_scope_guard_max_files_changed(),
            max_lines_added: default_pr_scope_guard_max_lines_added(),
        }
    }
}

impl Default for PrFeedbackPolicy {
    fn default() -> Self {
        Self {
            enabled: default_true(),
            sweep_interval_secs: default_feedback_sweep_interval_secs(),
            claim_stale_after_secs: default_feedback_claim_stale_after_secs(),
            hygiene_enabled: default_true(),
            hygiene_interval_secs: default_pr_hygiene_interval_secs(),
            dirty_age_to_repair_secs: default_pr_hygiene_dirty_age_to_repair_secs(),
            dirty_age_to_comment_secs: default_pr_hygiene_dirty_age_to_comment_secs(),
            rebase_needed_label: default_pr_hygiene_rebase_needed_label(),
            hygiene_batch_limit: default_pr_hygiene_batch_limit(),
        }
    }
}

impl Default for RuntimeDispatchPolicy {
    fn default() -> Self {
        Self {
            enabled: true,
            interval_secs: default_runtime_dispatch_interval_secs(),
            batch_limit: default_runtime_dispatch_batch_limit(),
            defer_backoff_secs: default_runtime_dispatch_defer_backoff_secs(),
            defer_backoff_max_secs: default_runtime_dispatch_defer_backoff_max_secs(),
            runtime_kind: None,
            runtime_profile: None,
            model: None,
            reasoning_effort: None,
            sandbox: None,
            approval_policy: None,
            max_turns: None,
            timeout_secs: None,
            workflow_profiles: BTreeMap::new(),
            activity_profiles: default_runtime_dispatch_activity_profiles(),
            workflow_activity_profiles: BTreeMap::new(),
        }
    }
}

impl RuntimeDispatchPolicy {
    fn apply_default_activity_profiles(&mut self) {
        for (activity, default_profile) in default_runtime_dispatch_activity_profiles() {
            self.activity_profiles
                .entry(activity)
                .and_modify(|profile| profile.apply_defaults_from(&default_profile))
                .or_insert(default_profile);
        }
    }
}

impl RuntimeDispatchProfileOverride {
    fn apply_defaults_from(&mut self, default: &Self) {
        if self.runtime_kind.is_none() {
            self.runtime_kind = default.runtime_kind.clone();
        }
        if self.runtime_profile.is_none() {
            self.runtime_profile = default.runtime_profile.clone();
        }
        if self.model.is_none() {
            self.model = default.model.clone();
        }
        if self.reasoning_effort.is_none() {
            self.reasoning_effort = default.reasoning_effort.clone();
        }
        if self.sandbox.is_none() {
            self.sandbox = default.sandbox.clone();
        }
        if self.approval_policy.is_none() {
            self.approval_policy = default.approval_policy.clone();
        }
        if self.max_turns.is_none() {
            self.max_turns = default.max_turns;
        }
        if self.timeout_secs.is_none() {
            self.timeout_secs = default.timeout_secs;
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

fn default_pr_scope_guard_max_files_changed() -> u32 {
    30
}

fn default_pr_scope_guard_max_lines_added() -> u32 {
    1500
}

fn default_feedback_sweep_interval_secs() -> u64 {
    60
}

fn default_feedback_claim_stale_after_secs() -> u64 {
    300
}

fn default_pr_hygiene_interval_secs() -> u64 {
    30 * 60
}

fn default_pr_hygiene_dirty_age_to_repair_secs() -> u64 {
    48 * 60 * 60
}

fn default_pr_hygiene_dirty_age_to_comment_secs() -> u64 {
    7 * 24 * 60 * 60
}

fn default_pr_hygiene_rebase_needed_label() -> String {
    "rebase-needed".to_string()
}

fn default_pr_hygiene_batch_limit() -> u32 {
    25
}

fn default_runtime_dispatch_interval_secs() -> u64 {
    30
}

fn default_runtime_dispatch_batch_limit() -> u32 {
    25
}

fn default_runtime_dispatch_defer_backoff_secs() -> u64 {
    30
}

fn default_runtime_dispatch_defer_backoff_max_secs() -> u64 {
    15 * 60
}

fn default_runtime_dispatch_activity_profiles() -> BTreeMap<String, RuntimeDispatchProfileOverride>
{
    BTreeMap::new()
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

/// Central base `WORKFLOW.md` path (the file that lives next to the loaded
/// server config, e.g. `config/WORKFLOW.md`). Registered once at server startup.
static WORKFLOW_BASE_PATH: OnceLock<PathBuf> = OnceLock::new();

/// Register the central base `WORKFLOW.md` (sibling of the loaded config file).
///
/// This is the single source of default workflow policy. Per-repo
/// `{project_root}/WORKFLOW.md` files are deep-merged on top of this base
/// field-by-field, so a managed repository only needs a WORKFLOW.md when it
/// wants to override specific fields; a repo with no WORKFLOW.md inherits the
/// base entirely. Set once at startup; subsequent calls are ignored.
pub fn set_workflow_base_path(path: PathBuf) {
    let _ = WORKFLOW_BASE_PATH.set(path);
}

fn workflow_base_path() -> Option<&'static Path> {
    WORKFLOW_BASE_PATH.get().map(PathBuf::as_path)
}

/// Parse a single `WORKFLOW.md` into its front-matter YAML value and prompt body.
/// Returns `Ok(None)` when the file does not exist.
fn read_workflow_file(path: &Path) -> anyhow::Result<Option<(serde_yaml::Value, String)>> {
    let contents = match std::fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let (front_matter, body) = split_front_matter_and_body(&contents);
    let value = match front_matter {
        None => serde_yaml::Value::Null,
        Some(front_matter) if front_matter.trim().is_empty() => serde_yaml::Value::Null,
        Some(front_matter) => serde_yaml::from_str(front_matter).map_err(|e| {
            anyhow::anyhow!(
                "failed to parse workflow front matter at {}: {e}",
                path.display()
            )
        })?,
    };
    Ok(Some((value, body.trim().to_string())))
}

/// Recursively merge `over` onto `base`. Mappings are merged key-by-key
/// (recursing into nested mappings); a YAML `null` override keeps the base
/// value; every other scalar/sequence override replaces the base value. This
/// gives field-level override semantics for nested workflow policy.
fn deep_merge_yaml(base: serde_yaml::Value, over: serde_yaml::Value) -> serde_yaml::Value {
    use serde_yaml::Value;
    match (base, over) {
        (Value::Mapping(mut base_map), Value::Mapping(over_map)) => {
            for (key, over_value) in over_map {
                let merged = match base_map.remove(&key) {
                    Some(base_value) => deep_merge_yaml(base_value, over_value),
                    None => over_value,
                };
                base_map.insert(key, merged);
            }
            Value::Mapping(base_map)
        }
        // An explicit null override means "not set" — preserve the base value.
        (base, Value::Null) => base,
        // Scalars and sequences: the override wins outright.
        (_, over) => over,
    }
}

/// Load the workflow policy and prompt template for `project_root`.
///
/// Resolution: the central base `WORKFLOW.md` (registered via
/// [`set_workflow_base_path`], normally `config/WORKFLOW.md` next to the server
/// config) supplies defaults, and `{project_root}/WORKFLOW.md` — when present —
/// is deep-merged on top with field-level override. Either file may be absent:
/// if both are missing, defaults are returned with an empty prompt template.
/// The repo body overrides the base body only when the repo provides one.
pub fn load_workflow_document(project_root: &Path) -> anyhow::Result<WorkflowDocument> {
    load_workflow_document_with_base(project_root, workflow_base_path())
}

/// Core resolution with an explicit base path (kept separate from the global
/// registration so it can be unit-tested without touching process state).
fn load_workflow_document_with_base(
    project_root: &Path,
    base_path: Option<&Path>,
) -> anyhow::Result<WorkflowDocument> {
    let repo_path = project_root.join("WORKFLOW.md");
    let repo = read_workflow_file(&repo_path)?;

    // The base only applies when it is a distinct file from the repo's own
    // WORKFLOW.md (otherwise a repo that *is* the config dir would merge with
    // itself).
    let base = match base_path {
        Some(base_path) if workflow_paths_are_distinct(base_path, &repo_path) => {
            read_workflow_file(base_path)?.map(|loaded| (base_path, loaded))
        }
        _ => None,
    };

    let (merged_value, prompt_template, source_path) = match (base, repo) {
        (None, None) => return Ok(WorkflowDocument::default()),
        (Some((base_path, (base_value, base_body))), None) => {
            (base_value, base_body, base_path.display().to_string())
        }
        (None, Some((repo_value, repo_body))) => {
            (repo_value, repo_body, repo_path.display().to_string())
        }
        (Some((base_path, (base_value, base_body))), Some((repo_value, repo_body))) => {
            let merged = deep_merge_yaml(base_value, repo_value);
            let body = if repo_body.is_empty() {
                base_body
            } else {
                repo_body
            };
            let source = format!("{} + {}", base_path.display(), repo_path.display());
            (merged, body, source)
        }
    };

    let mut config: WorkflowConfig = match merged_value {
        serde_yaml::Value::Null => WorkflowConfig::default(),
        value => serde_yaml::from_value(value).map_err(|e| {
            anyhow::anyhow!("failed to parse merged workflow front matter ({source_path}): {e}")
        })?,
    };
    config.runtime_dispatch.apply_default_activity_profiles();
    if config.runtime_dispatch.defer_backoff_secs == 0
        || config.runtime_dispatch.defer_backoff_max_secs
            < config.runtime_dispatch.defer_backoff_secs
        || config.runtime_dispatch.defer_backoff_secs > i64::MAX as u64
        || config.runtime_dispatch.defer_backoff_max_secs > i64::MAX as u64
    {
        anyhow::bail!(
            "runtime_dispatch defer backoff requires positive BIGINT-compatible seconds and max >= floor"
        );
    }

    Ok(WorkflowDocument {
        config,
        prompt_template,
        source_path: Some(source_path),
    })
}

fn workflow_paths_are_distinct(left: &Path, right: &Path) -> bool {
    workflow_path_identity(left) != workflow_path_identity(right)
}

fn workflow_path_identity(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
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
