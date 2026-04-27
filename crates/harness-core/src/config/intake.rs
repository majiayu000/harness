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
}

/// GitHub Issues intake configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitHubIntakeConfig {
    /// Enable polling GitHub Issues for tasks. Default: false.
    #[serde(default)]
    pub enabled: bool,
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
                })
            })
    }
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

impl Default for GitHubIntakeConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            repo: String::new(),
            label: default_intake_label(),
            poll_interval_secs: default_poll_interval_secs(),
            repos: Vec::new(),
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
mod tests {
    use super::*;

    #[test]
    fn feishu_debug_redacts_secrets() {
        let config = FeishuIntakeConfig {
            enabled: true,
            app_id: Some("app-123".to_string()),
            app_secret: Some("super-secret".to_string()),
            verification_token: Some("tok-abc".to_string()),
            trigger_keyword: "harness".to_string(),
            default_repo: Some("owner/repo".to_string()),
        };
        let debug_output = format!("{config:?}");
        assert!(
            !debug_output.contains("app-123"),
            "app_id must not appear in Debug output"
        );
        assert!(
            !debug_output.contains("super-secret"),
            "app_secret must not appear in Debug output"
        );
        assert!(
            !debug_output.contains("tok-abc"),
            "verification_token must not appear in Debug output"
        );
        assert!(
            debug_output.contains("[REDACTED]"),
            "Debug output must contain [REDACTED]"
        );
    }

    #[test]
    fn feishu_debug_shows_none_for_absent_secrets() {
        let config = FeishuIntakeConfig::default();
        let debug_output = format!("{config:?}");
        assert!(
            debug_output.contains("None"),
            "absent secrets should show as None"
        );
    }

    #[test]
    fn github_find_repo_config_prefers_exact_multi_repo_match() {
        let config = GitHubIntakeConfig {
            repo: "owner/default".to_string(),
            repos: vec![
                GitHubRepoConfig {
                    repo: "owner/default".to_string(),
                    label: "default".to_string(),
                    project_root: Some("/srv/default".to_string()),
                },
                GitHubRepoConfig {
                    repo: "owner/other".to_string(),
                    label: "other".to_string(),
                    project_root: Some("/srv/other".to_string()),
                },
            ],
            ..GitHubIntakeConfig::default()
        };

        let repo = config
            .find_repo_config("owner/other")
            .expect("repo config should exist");
        assert_eq!(repo.repo, "owner/other");
        assert_eq!(repo.label, "other");
        assert_eq!(repo.project_root.as_deref(), Some("/srv/other"));
    }

    #[test]
    fn github_find_repo_config_supports_single_repo_shorthand() {
        let config = GitHubIntakeConfig {
            repo: "owner/default".to_string(),
            label: "triage".to_string(),
            ..GitHubIntakeConfig::default()
        };

        let repo = config
            .find_repo_config("owner/default")
            .expect("repo config should exist");
        assert_eq!(repo.repo, "owner/default");
        assert_eq!(repo.label, "triage");
        assert_eq!(repo.project_root, None);
    }
}

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
