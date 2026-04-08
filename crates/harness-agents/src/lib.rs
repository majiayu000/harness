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
///
/// A 5-second wall-clock timeout is enforced: if `claude --help` does not exit
/// within that window (e.g. bad wrapper script, blocked NFS mount, first-run
/// interactive prompt), the child is killed and the probe returns `false`.
pub(crate) fn probe_no_session_persistence(cli_path: &std::path::Path) -> bool {
    // Validate cli_path before spawning: must be absolute and an existing regular
    // file.  This prevents an unsandboxed execution of an arbitrary/misconfigured
    // path before wrap_command sandboxing has been applied.
    if !cli_path.is_absolute() {
        tracing::warn!(
            path = %cli_path.display(),
            "probe_no_session_persistence: cli_path is not absolute; skipping probe"
        );
        return false;
    }
    match std::fs::metadata(cli_path) {
        Ok(meta) if meta.is_file() => {}
        Ok(_) => {
            tracing::warn!(
                path = %cli_path.display(),
                "probe_no_session_persistence: cli_path is not a regular file; skipping probe"
            );
            return false;
        }
        Err(e) => {
            tracing::warn!(
                path = %cli_path.display(),
                error = %e,
                "probe_no_session_persistence: cannot stat cli_path; skipping probe"
            );
            return false;
        }
    }

    let claude_keys: Vec<String> = std::env::vars()
        .filter(|(k, _)| k.starts_with("CLAUDE"))
        .map(|(k, _)| k)
        .collect();
    let mut cmd = std::process::Command::new(cli_path);
    cmd.arg("--help")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    for key in &claude_keys {
        cmd.env_remove(key);
    }

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(_) => return false,
    };

    // Drain stdout/stderr in background threads to prevent pipe-buffer deadlock
    // when the help output is large.
    use std::io::Read;
    let (tx_out, rx_out) = std::sync::mpsc::channel();
    let (tx_err, rx_err) = std::sync::mpsc::channel();
    if let Some(mut stdout) = child.stdout.take() {
        std::thread::spawn(move || {
            let mut buf = String::new();
            if let Err(e) = stdout.read_to_string(&mut buf) {
                tracing::warn!("probe_no_session_persistence: failed to read stdout: {e}");
            }
            // Send is fire-and-forget; receiver may have already gone on timeout.
            if tx_out.send(buf).is_err() {
                tracing::debug!(
                    "probe_no_session_persistence: stdout receiver dropped (probe timed out)"
                );
            }
        });
    }
    if let Some(mut stderr) = child.stderr.take() {
        std::thread::spawn(move || {
            let mut buf = String::new();
            if let Err(e) = stderr.read_to_string(&mut buf) {
                tracing::warn!("probe_no_session_persistence: failed to read stderr: {e}");
            }
            if tx_err.send(buf).is_err() {
                tracing::debug!(
                    "probe_no_session_persistence: stderr receiver dropped (probe timed out)"
                );
            }
        });
    }

    // Poll for exit with a 5-second deadline; kill and return false on timeout.
    const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
    let deadline = std::time::Instant::now() + PROBE_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    if let Err(e) = child.kill() {
                        tracing::warn!(
                            path = %cli_path.display(),
                            "probe_no_session_persistence: failed to kill timed-out process: {e}"
                        );
                    }
                    tracing::warn!(
                        path = %cli_path.display(),
                        "probe_no_session_persistence: claude --help timed out after 5 s; \
                         treating --no-session-persistence as unsupported"
                    );
                    return false;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Err(e) => {
                if let Err(kill_err) = child.kill() {
                    tracing::warn!(
                        "probe_no_session_persistence: failed to kill process after wait error \
                         ({e}): {kill_err}"
                    );
                }
                return false;
            }
        }
    }

    // Collect output — background threads finish once pipes close on process exit.
    let one_sec = std::time::Duration::from_secs(1);
    let stdout_content = match rx_out.recv_timeout(one_sec) {
        Ok(s) => s,
        Err(e) => {
            tracing::debug!("probe_no_session_persistence: stdout not received within 1 s: {e}");
            String::new()
        }
    };
    let stderr_content = match rx_err.recv_timeout(one_sec) {
        Ok(s) => s,
        Err(e) => {
            tracing::debug!("probe_no_session_persistence: stderr not received within 1 s: {e}");
            String::new()
        }
    };
    stdout_content.contains("--no-session-persistence")
        || stderr_content.contains("--no-session-persistence")
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
