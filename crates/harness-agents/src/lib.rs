pub mod anthropic_api;
pub mod claude;
pub mod claude_adapter;
mod cloud_setup;
pub mod codex;
pub mod codex_adapter;
pub mod registry;
mod streaming;

pub use registry::{AdapterRegistry, AgentRegistry};

/// Remove all `CLAUDE`-prefixed environment variables from a command to prevent
/// nested Claude Code detection (SIGTRAP).
pub(crate) fn strip_claude_env(cmd: &mut tokio::process::Command) {
    let claude_keys: Vec<String> = std::env::vars()
        .filter(|(k, _)| k.starts_with("CLAUDE"))
        .map(|(k, _)| k)
        .collect();
    for key in &claude_keys {
        cmd.env_remove(key);
    }
}
