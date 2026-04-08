pub mod anthropic_api;
pub mod claude;
pub mod claude_adapter;
mod cloud_setup;
pub mod codex;
pub mod codex_adapter;
pub mod registry;
mod streaming;

/// Probe whether the Claude CLI at `cli_path` supports `--no-session-persistence`.
///
/// Runs `cli_path --help` once and checks the combined stdout/stderr output.
/// Returns `false` on any spawn or read error so that agents on older CLI builds
/// degrade gracefully (flag omitted) rather than failing every spawn with
/// "unknown option --no-session-persistence".
///
/// All `CLAUDE`-prefixed environment variables are stripped before spawning to
/// prevent nested-Claude SIGTRAP in environments launched from within Claude Code.
pub(crate) fn probe_no_session_persistence(cli_path: &std::path::Path) -> bool {
    let claude_keys: Vec<String> = std::env::vars()
        .filter(|(k, _)| k.starts_with("CLAUDE"))
        .map(|(k, _)| k)
        .collect();
    let mut cmd = std::process::Command::new(cli_path);
    cmd.arg("--help");
    for key in &claude_keys {
        cmd.env_remove(key);
    }
    cmd.output()
        .map(|out| {
            let stdout = String::from_utf8_lossy(&out.stdout);
            let stderr = String::from_utf8_lossy(&out.stderr);
            stdout.contains("--no-session-persistence")
                || stderr.contains("--no-session-persistence")
        })
        .unwrap_or(false)
}

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
