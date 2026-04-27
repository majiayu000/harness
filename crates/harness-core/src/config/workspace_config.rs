use super::dirs::dirs_data_dir;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Workspace isolation configuration for parallel task execution.
///
/// WorkspaceManager provisions an isolated git worktree per task,
/// preventing merge conflicts when multiple agents edit the same project.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceConfig {
    /// Root directory for all task worktrees. Default: `~/.local/share/harness/workspaces`.
    #[serde(default = "default_workspace_root")]
    pub root: PathBuf,
    /// Shell script run after worktree creation (cwd = workspace). Fatal on failure.
    #[serde(default)]
    pub after_create_hook: Option<String>,
    /// Shell script run before worktree removal (cwd = workspace). Non-fatal on failure.
    #[serde(default)]
    pub before_remove_hook: Option<String>,
    /// Timeout in seconds for hook execution. Default: 60.
    #[serde(default = "default_hook_timeout_secs")]
    pub hook_timeout_secs: u64,
    /// If true, remove workspace when task reaches Done or Failed state. Default: true.
    #[serde(default = "default_auto_cleanup")]
    pub auto_cleanup: bool,
}

fn default_workspace_root() -> PathBuf {
    dirs_data_dir().join("harness").join("workspaces")
}

fn default_hook_timeout_secs() -> u64 {
    60
}

fn default_auto_cleanup() -> bool {
    true
}

impl Default for WorkspaceConfig {
    fn default() -> Self {
        Self {
            root: default_workspace_root(),
            after_create_hook: None,
            before_remove_hook: None,
            hook_timeout_secs: default_hook_timeout_secs(),
            auto_cleanup: default_auto_cleanup(),
        }
    }
}
