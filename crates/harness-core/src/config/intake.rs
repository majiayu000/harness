use std::fmt;

use serde::{Deserialize, Serialize};

/// Per-repo GitHub intake entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitHubRepoConfig {
    /// GitHub repository in "owner/repo" format.
    pub repo: String,
    /// Issue label to filter on. Default: "harness".
    #[serde(default = "default_intake_label")]
    pub label: String,
    /// Project root to associate with issues from this repo.
    pub project_root: Option<String>,
    /// Explicit per-repo auto-merge opt-in. When omitted, the global
    /// `[intake.github.auto_merge]` policy applies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_merge: Option<bool>,
    /// Explicit per-repo auto-recovery opt-in. When omitted, the global
    /// `[intake.github.auto_recovery]` enabled flag applies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_recovery: Option<bool>,
    /// Optional per-repo merge method override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merge_method: Option<GitHubMergeMethod>,
    /// Optional per-repo branch cleanup override after merge.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delete_branch: Option<bool>,
    /// Optional per-repo review-thread readiness override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub require_review_threads_resolved: Option<bool>,
    /// Optional per-repo mergeability readiness override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub require_clean_merge_state: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum GitHubMergeMethod {
    #[default]
    Squash,
    Merge,
    Rebase,
}

impl fmt::Display for GitHubMergeMethod {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Squash => f.write_str("squash"),
            Self::Merge => f.write_str("merge"),
            Self::Rebase => f.write_str("rebase"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum GitHubMergeExecution {
    #[default]
    Agent,
    Server,
}

impl fmt::Display for GitHubMergeExecution {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Agent => f.write_str("agent"),
            Self::Server => f.write_str("server"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitHubAutoMergeConfig {
    /// Explicitly enable automatic merge dispatch. Default: false.
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub method: GitHubMergeMethod,
    #[serde(default = "default_true")]
    pub delete_branch: bool,
    #[serde(default = "default_true")]
    pub require_review_threads_resolved: bool,
    #[serde(default = "default_true")]
    pub require_clean_merge_state: bool,
    /// Who executes the merge action once the server-side gate passes.
    /// Default is agent execution with server-side completion verification.
    #[serde(default)]
    pub merge_execution: GitHubMergeExecution,
    /// Verify agent-reported merge completion with a server-side GitHub read.
    /// Default true; disabling restores the legacy trust-agent behavior.
    #[serde(default = "default_true")]
    pub verify_merge_completion: bool,
}

/// Opt-in automatic recovery policy for stopped (`blocked` / `failed`)
/// workflow-runtime instances whose stop reason classifies as transient
/// (GH-1584). Disabled by default; repos opt in with `auto_recovery = true`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GitHubAutoRecoveryConfig {
    /// Explicitly enable the auto-recovery recheck scheduler. Default: false.
    #[serde(default)]
    pub enabled: bool,
    /// Maximum automatic recovery attempts per stop episode. Default: 3.
    #[serde(default = "default_auto_recovery_max_attempts")]
    pub max_attempts: u32,
    /// Base backoff before the first recheck, in seconds. Default: 300.
    #[serde(default = "default_auto_recovery_initial_backoff_secs")]
    pub initial_backoff_secs: u64,
    /// Backoff ceiling in seconds. Default: 14400 (4 hours).
    #[serde(default = "default_auto_recovery_max_backoff_secs")]
    pub max_backoff_secs: u64,
    /// Jitter ratio applied to each backoff, in [0, 1]. Default: 0.2.
    #[serde(default = "default_auto_recovery_jitter_ratio")]
    pub jitter_ratio: f64,
    /// Scheduler tick interval in seconds. Default: 60.
    #[serde(default = "default_auto_recovery_tick_interval_secs")]
    pub tick_interval_secs: u64,
}

/// Upper bound on `max_attempts`; combined with saturating arithmetic it
/// rules out integer overflow in the `2^attempts` backoff computation.
pub const AUTO_RECOVERY_MAX_ATTEMPTS_CEILING: u32 = 16;

impl GitHubAutoRecoveryConfig {
    /// Validate policy bounds at config load (B-016) instead of recheck time.
    pub fn validate(&self) -> anyhow::Result<()> {
        if !self.enabled {
            return Ok(());
        }
        if self.max_attempts == 0 || self.max_attempts > AUTO_RECOVERY_MAX_ATTEMPTS_CEILING {
            anyhow::bail!(
                "[intake.github.auto_recovery] max_attempts must be between 1 and {} when enabled, got {}",
                AUTO_RECOVERY_MAX_ATTEMPTS_CEILING,
                self.max_attempts
            );
        }
        if self.initial_backoff_secs > self.max_backoff_secs {
            anyhow::bail!(
                "[intake.github.auto_recovery] max_backoff_secs ({}) must be >= initial_backoff_secs ({})",
                self.max_backoff_secs,
                self.initial_backoff_secs
            );
        }
        if !(0.0..=1.0).contains(&self.jitter_ratio) || self.jitter_ratio.is_nan() {
            anyhow::bail!(
                "[intake.github.auto_recovery] jitter_ratio must be within [0, 1], got {}",
                self.jitter_ratio
            );
        }
        if self.tick_interval_secs == 0 {
            anyhow::bail!("[intake.github.auto_recovery] tick_interval_secs must be >= 1");
        }
        Ok(())
    }
}

impl Default for GitHubAutoRecoveryConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            max_attempts: default_auto_recovery_max_attempts(),
            initial_backoff_secs: default_auto_recovery_initial_backoff_secs(),
            max_backoff_secs: default_auto_recovery_max_backoff_secs(),
            jitter_ratio: default_auto_recovery_jitter_ratio(),
            tick_interval_secs: default_auto_recovery_tick_interval_secs(),
        }
    }
}

fn default_auto_recovery_max_attempts() -> u32 {
    3
}

fn default_auto_recovery_initial_backoff_secs() -> u64 {
    300
}

fn default_auto_recovery_max_backoff_secs() -> u64 {
    14400
}

fn default_auto_recovery_jitter_ratio() -> f64 {
    0.2
}

fn default_auto_recovery_tick_interval_secs() -> u64 {
    60
}

/// How GitHub issue intake is driven.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum IntakeMode {
    /// Autonomous polling discovers and works open issues (legacy default).
    #[default]
    Poll,
    /// Event-driven: the `/webhook` endpoint auto-enqueues `issues`
    /// opened/reopened events (no `@harness` mention required) and the
    /// GitHub intake poller is skipped.
    Webhook,
    /// Both: webhook auto-enqueues for immediacy, poller runs as a backstop
    /// so events missed during downtime are still picked up.
    Hybrid,
}

impl IntakeMode {
    /// Whether the webhook should auto-enqueue plain issue events (no mention).
    pub fn webhook_autonomous(self) -> bool {
        matches!(self, IntakeMode::Webhook | IntakeMode::Hybrid)
    }

    /// Whether the GitHub intake poller should run.
    pub fn poller_enabled(self) -> bool {
        matches!(self, IntakeMode::Poll | IntakeMode::Hybrid)
    }
}

/// How poll-capable GitHub intake discovers repository backlog.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum GitHubPollDiscoveryDriver {
    /// Discover issues through direct GitHub REST calls without an agent.
    #[default]
    DirectRest,
    /// Reserve a richer agent-driven backlog analysis path for opt-in use.
    Agent,
}

impl fmt::Display for GitHubPollDiscoveryDriver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DirectRest => f.write_str("direct_rest"),
            Self::Agent => f.write_str("agent"),
        }
    }
}

/// GitHub Issues intake configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitHubIntakeConfig {
    /// Enable polling GitHub Issues for tasks. Default: false.
    #[serde(default)]
    pub enabled: bool,
    /// Intake driver mode: `poll` (autonomous polling), `webhook` (event-driven,
    /// poller off), or `hybrid` (both). Default: `poll`.
    #[serde(default)]
    pub mode: IntakeMode,
    /// Poll-capable backlog discovery driver. Default: `direct_rest`.
    #[serde(default)]
    pub discovery_driver: GitHubPollDiscoveryDriver,
    /// Single repo (backward compat shorthand).
    #[serde(default)]
    pub repo: String,
    /// Issue label to filter on (used with single `repo`). Default: "harness".
    #[serde(default = "default_intake_label")]
    pub label: String,
    /// Polling interval in seconds. Default: 30.
    #[serde(default = "default_poll_interval_secs")]
    pub poll_interval_secs: u64,
    /// Multiple repos to poll.
    #[serde(default)]
    pub repos: Vec<GitHubRepoConfig>,
    /// Global auto-merge policy. Disabled by default; repos can opt in with
    /// `auto_merge = true`.
    #[serde(default)]
    pub auto_merge: GitHubAutoMergeConfig,
    /// Global auto-recovery policy for stopped workflow-runtime instances.
    /// Disabled by default; repos can opt in with `auto_recovery = true`.
    #[serde(default)]
    pub auto_recovery: GitHubAutoRecoveryConfig,
    /// Agent name for the sprint planner. None = use server default.
    #[serde(default)]
    pub planner_agent: Option<String>,
    /// Maximum seconds to wait for sprint planning / coordination before the
    /// sprint is abandoned for this poll cycle. Default: 10800 (3 hours).
    #[serde(default = "default_sprint_timeout_secs")]
    pub sprint_timeout_secs: u64,
    /// Base seconds for exponential backoff after transient enqueue failures.
    /// Default: 15.
    #[serde(default = "default_retry_backoff_base_secs")]
    pub retry_backoff_base_secs: u64,
    /// Maximum seconds for capped exponential backoff after transient enqueue
    /// failures. Default: 120.
    #[serde(default = "default_retry_backoff_max_secs")]
    pub retry_backoff_max_secs: u64,
}

/// Expand config into a flat list of (repo, label, project_root) tuples.
/// Merges the single `repo` field with the `repos` array.
impl GitHubIntakeConfig {
    pub fn effective_repos(&self) -> Vec<GitHubRepoConfig> {
        let mut result: Vec<GitHubRepoConfig> = self.repos.clone();
        if !self.repo.is_empty() {
            let already = result.iter().any(|r| r.repo == self.repo);
            if !already {
                result.push(GitHubRepoConfig {
                    repo: self.repo.clone(),
                    label: self.label.clone(),
                    project_root: None,
                    auto_merge: None,
                    auto_recovery: None,
                    merge_method: None,
                    delete_branch: None,
                    require_review_threads_resolved: None,
                    require_clean_merge_state: None,
                });
            }
        }
        result
    }

    pub fn find_repo_config(&self, repo: &str) -> Option<GitHubRepoConfig> {
        self.repos
            .iter()
            .find(|config| config.repo == repo)
            .cloned()
            .or_else(|| {
                (self.repo == repo).then(|| GitHubRepoConfig {
                    repo: self.repo.clone(),
                    label: self.label.clone(),
                    project_root: None,
                    auto_merge: None,
                    auto_recovery: None,
                    merge_method: None,
                    delete_branch: None,
                    require_review_threads_resolved: None,
                    require_clean_merge_state: None,
                })
            })
    }

    /// Effective auto-recovery opt-in for `repo`: the per-repo override wins,
    /// otherwise the global `[intake.github.auto_recovery]` flag applies.
    /// Repos that are not configured never opt in.
    pub fn auto_recovery_enabled_for_repo(&self, repo: &str) -> bool {
        let Some(repo_cfg) = self.find_repo_config(repo) else {
            return false;
        };
        repo_cfg.auto_recovery.unwrap_or(self.auto_recovery.enabled)
    }

    pub fn auto_merge_policy_for_repo(&self, repo: &str) -> ResolvedGitHubAutoMergePolicy {
        let repo_cfg = self.find_repo_config(repo);
        let repo_cfg = repo_cfg.as_ref();
        ResolvedGitHubAutoMergePolicy {
            enabled: repo_cfg
                .and_then(|config| config.auto_merge)
                .unwrap_or(self.auto_merge.enabled),
            method: repo_cfg
                .and_then(|config| config.merge_method)
                .unwrap_or(self.auto_merge.method),
            delete_branch: repo_cfg
                .and_then(|config| config.delete_branch)
                .unwrap_or(self.auto_merge.delete_branch),
            require_review_threads_resolved: repo_cfg
                .and_then(|config| config.require_review_threads_resolved)
                .unwrap_or(self.auto_merge.require_review_threads_resolved),
            require_clean_merge_state: repo_cfg
                .and_then(|config| config.require_clean_merge_state)
                .unwrap_or(self.auto_merge.require_clean_merge_state),
            merge_execution: self.auto_merge.merge_execution,
            verify_merge_completion: self.auto_merge.verify_merge_completion,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedGitHubAutoMergePolicy {
    pub enabled: bool,
    pub method: GitHubMergeMethod,
    pub delete_branch: bool,
    pub require_review_threads_resolved: bool,
    pub require_clean_merge_state: bool,
    pub merge_execution: GitHubMergeExecution,
    pub verify_merge_completion: bool,
}

fn default_intake_label() -> String {
    "harness".to_string()
}

fn default_poll_interval_secs() -> u64 {
    30
}

fn default_sprint_timeout_secs() -> u64 {
    3 * 60 * 60
}

fn default_retry_backoff_base_secs() -> u64 {
    15
}

fn default_retry_backoff_max_secs() -> u64 {
    120
}

fn default_true() -> bool {
    true
}

impl Default for GitHubAutoMergeConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            method: GitHubMergeMethod::Squash,
            delete_branch: true,
            require_review_threads_resolved: true,
            require_clean_merge_state: true,
            merge_execution: GitHubMergeExecution::Agent,
            verify_merge_completion: true,
        }
    }
}

impl Default for GitHubIntakeConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            mode: IntakeMode::default(),
            discovery_driver: GitHubPollDiscoveryDriver::default(),
            repo: String::new(),
            label: default_intake_label(),
            poll_interval_secs: default_poll_interval_secs(),
            repos: Vec::new(),
            auto_merge: GitHubAutoMergeConfig::default(),
            auto_recovery: GitHubAutoRecoveryConfig::default(),
            planner_agent: None,
            sprint_timeout_secs: default_sprint_timeout_secs(),
            retry_backoff_base_secs: default_retry_backoff_base_secs(),
            retry_backoff_max_secs: default_retry_backoff_max_secs(),
        }
    }
}

/// Feishu Bot intake configuration.
#[derive(Clone, Serialize, Deserialize)]
pub struct FeishuIntakeConfig {
    /// Enable Feishu bot webhook. Default: false.
    #[serde(default)]
    pub enabled: bool,
    /// Feishu app_id. Falls back to $FEISHU_APP_ID env var.
    pub app_id: Option<String>,
    /// Feishu app_secret. Falls back to $FEISHU_APP_SECRET env var.
    pub app_secret: Option<String>,
    /// Webhook verification token for challenge handshake.
    pub verification_token: Option<String>,
    /// Keyword in message text that triggers task creation. Default: "harness".
    #[serde(default = "default_feishu_trigger_keyword")]
    pub trigger_keyword: String,
    /// Default repository to work on (e.g. "owner/repo").
    pub default_repo: Option<String>,
}

fn default_feishu_trigger_keyword() -> String {
    "harness".to_string()
}

impl Default for FeishuIntakeConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            app_id: None,
            app_secret: None,
            verification_token: None,
            trigger_keyword: default_feishu_trigger_keyword(),
            default_repo: None,
        }
    }
}

impl fmt::Debug for FeishuIntakeConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FeishuIntakeConfig")
            .field("enabled", &self.enabled)
            .field("app_id", &self.app_id.as_ref().map(|_| "[REDACTED]"))
            .field(
                "app_secret",
                &self.app_secret.as_ref().map(|_| "[REDACTED]"),
            )
            .field(
                "verification_token",
                &self.verification_token.as_ref().map(|_| "[REDACTED]"),
            )
            .field("trigger_keyword", &self.trigger_keyword)
            .field("default_repo", &self.default_repo)
            .finish()
    }
}

#[cfg(test)]
mod tests;

/// Multi-channel task intake configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IntakeConfig {
    /// GitHub Issues poller configuration.
    #[serde(default)]
    pub github: Option<GitHubIntakeConfig>,
    /// Feishu Bot webhook configuration.
    #[serde(default)]
    pub feishu: Option<FeishuIntakeConfig>,
}
