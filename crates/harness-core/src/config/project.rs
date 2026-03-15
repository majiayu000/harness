use serde::{Deserialize, Serialize};
use std::path::Path;

/// Per-project agent configuration overrides.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProjectAgentConfig {
    /// Override the default agent name for this project.
    #[serde(default)]
    pub default: Option<String>,
}

/// Per-project review configuration overrides.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProjectReviewConfig {
    /// Whether agent review is enabled for this project.
    #[serde(default)]
    pub enabled: Option<bool>,
    /// Review bot command override (e.g. "/gemini review").
    #[serde(default)]
    pub bot_command: Option<String>,
    /// GitHub login of the review bot user override (e.g. "gemini-code-assist[bot]").
    /// When absent, the server default reviewer_name is inherited unchanged.
    /// If `bot_command` is changed to a bot with a different GitHub login,
    /// this field must also be set explicitly.
    #[serde(default)]
    pub reviewer_name: Option<String>,
}

/// Per-project concurrency configuration overrides.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProjectConcurrencyConfig {
    /// Maximum number of concurrent tasks for this project.
    #[serde(default)]
    pub max_concurrent_tasks: Option<usize>,
}

/// Per-project GC configuration overrides.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProjectGcConfig {
    /// Maximum number of drafts per GC run for this project.
    #[serde(default)]
    pub max_drafts_per_run: Option<usize>,
}

/// Git configuration for a project.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitConfig {
    /// Base branch to target PRs against (e.g. "main", "develop").
    #[serde(default = "default_base_branch")]
    pub base_branch: String,
    /// Git remote name (e.g. "origin", "upstream").
    #[serde(default = "default_git_remote")]
    pub remote: String,
    /// Prefix for feature branches created by agents (e.g. "feat/", "fix/").
    #[serde(default = "default_branch_prefix")]
    pub branch_prefix: String,
}

fn default_base_branch() -> String {
    "main".to_string()
}

fn default_git_remote() -> String {
    "origin".to_string()
}

fn default_branch_prefix() -> String {
    "feat/".to_string()
}

impl Default for GitConfig {
    fn default() -> Self {
        Self {
            base_branch: default_base_branch(),
            remote: default_git_remote(),
            branch_prefix: default_branch_prefix(),
        }
    }
}

/// Per-project configuration loaded from `.harness/config.toml`.
///
/// Placed in the project root under `.harness/config.toml`.  All fields are
/// optional — missing sections fall back to sensible defaults.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProjectConfig {
    /// Git settings: base branch, remote, and branch prefix for agents.
    #[serde(default)]
    pub git: GitConfig,
    /// Validation commands run after each agent turn.  Empty `pre_commit`
    /// triggers language detection (cargo fmt/check for Rust, etc.).
    #[serde(default)]
    pub validation: super::misc::ValidationConfig,
    /// Per-project agent overrides. `None` inherits from server defaults.
    #[serde(default)]
    pub agent: Option<ProjectAgentConfig>,
    /// Per-project review overrides. `None` inherits from server defaults.
    #[serde(default)]
    pub review: Option<ProjectReviewConfig>,
    /// Per-project concurrency overrides. `None` inherits from server defaults.
    #[serde(default)]
    pub concurrency: Option<ProjectConcurrencyConfig>,
    /// Per-project GC overrides. `None` inherits from server defaults.
    #[serde(default)]
    pub gc: Option<ProjectGcConfig>,
}

/// Load project config from `{project_root}/.harness/config.toml`.
///
/// Returns `ProjectConfig::default()` if the file does not exist or cannot
/// be parsed, so the caller never needs to handle an error.
pub fn load_project_config(project_root: &Path) -> ProjectConfig {
    let path = project_root.join(".harness").join("config.toml");
    let Ok(contents) = std::fs::read_to_string(&path) else {
        return ProjectConfig::default();
    };
    toml::from_str(&contents).unwrap_or_else(|e| {
        eprintln!(
            "warning: failed to parse project config at {}: {e}",
            path.display()
        );
        ProjectConfig::default()
    })
}
