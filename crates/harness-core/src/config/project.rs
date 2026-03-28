use anyhow::Context;
use serde::{Deserialize, Serialize};
use std::path::Path;

/// Project type used to select review focus criteria.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReviewType {
    /// Rust projects: focus on concurrency, deadlocks, error handling.
    Rust,
    /// Shell scripts: focus on quoting, portability, command injection.
    Shell,
    /// Documentation projects: focus on accuracy, broken links, completeness.
    Documentation,
    /// Mixed or unknown projects: general correctness and security review.
    #[default]
    Mixed,
}

impl ReviewType {
    pub fn as_str(self) -> &'static str {
        match self {
            ReviewType::Rust => "rust",
            ReviewType::Shell => "shell",
            ReviewType::Documentation => "documentation",
            ReviewType::Mixed => "mixed",
        }
    }
}

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
    /// Override the GitHub login of the external review bot used in freshness checks.
    /// Must match the `user.login` field returned by the GitHub reviews API.
    #[serde(default)]
    pub reviewer_name: Option<String>,
    /// Whether to auto-trigger and wait for external review bot (e.g. Gemini).
    /// Set to `false` for projects without an external review bot installed.
    #[serde(default)]
    pub review_bot_auto_trigger: Option<bool>,
    /// Override seconds to wait for the review bot before starting the review loop.
    #[serde(default)]
    pub review_wait_secs: Option<u64>,
    /// Override maximum review bot rounds for this project.
    #[serde(default)]
    pub review_max_rounds: Option<u32>,
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
    /// Project type used to select review focus criteria. Default: mixed.
    #[serde(default)]
    pub review_type: ReviewType,
}

/// Load project config from `{project_root}/.harness/config.toml`.
///
/// Returns `ProjectConfig::default()` when the file is absent.
/// Returns an error when the file exists but cannot be read or parsed.
pub fn load_project_config(project_root: &Path) -> anyhow::Result<ProjectConfig> {
    let path = project_root.join(".harness").join("config.toml");
    let contents = match std::fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(ProjectConfig::default()),
        Err(e) => {
            return Err(e)
                .with_context(|| format!("failed to read project config at {}", path.display()));
        }
    };
    toml::from_str(&contents)
        .with_context(|| format!("failed to parse project config at {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_project_config_returns_default_when_file_missing() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let config = load_project_config(dir.path())?;
        assert_eq!(config.git.base_branch, "main");
        assert_eq!(config.git.remote, "origin");
        assert_eq!(config.review_type, ReviewType::Mixed);
        Ok(())
    }

    #[test]
    fn load_project_config_returns_error_on_parse_failure() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let harness_dir = dir.path().join(".harness");
        std::fs::create_dir_all(&harness_dir)?;
        std::fs::write(harness_dir.join("config.toml"), "git = [")?;

        let err = load_project_config(dir.path()).expect_err("invalid toml should fail");
        let msg = err.to_string();
        assert!(
            msg.contains("failed to parse project config"),
            "unexpected error: {msg}"
        );
        Ok(())
    }

    #[test]
    fn load_project_config_reads_valid_file() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let harness_dir = dir.path().join(".harness");
        std::fs::create_dir_all(&harness_dir)?;
        std::fs::write(
            harness_dir.join("config.toml"),
            r#"
[git]
base_branch = "develop"
remote = "upstream"
branch_prefix = "featx/"

review_type = "rust"
"#,
        )?;

        let config = load_project_config(dir.path())?;
        assert_eq!(config.git.base_branch, "develop");
        assert_eq!(config.git.remote, "upstream");
        assert_eq!(config.git.branch_prefix, "featx/");
        Ok(())
    }
}
