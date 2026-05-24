use harness_core::lang_detect;
use harness_core::validation::{ValidationExecutor, ValidationOutcomeKind, ValidationRequest};
use std::collections::HashMap;
use std::time::Duration;

/// Run the project's test commands as a hard gate before accepting LGTM.
///
/// When `custom_cmds` is non-empty (from `validation.pre_push` in project config),
/// those commands are run in order instead of language-detected defaults.
/// When `custom_cmds` is empty, falls back to language detection.
///
/// Returns `Ok(())` when all commands pass or when no test command is detectable
/// (soft degradation — unknown project type skips rather than hard-fails).
///
/// Returns `Err(output)` containing stdout/stderr of the first failing command.
pub(crate) async fn run_test_gate(
    validation_executor: &dyn ValidationExecutor,
    project_root: &std::path::Path,
    custom_cmds: &[String],
    timeout_secs: u64,
    extra_env: &HashMap<String, String>,
) -> Result<(), String> {
    // Prefer explicitly configured pre_push commands; fall back to language detection.
    let cmds: Vec<String> = if !custom_cmds.is_empty() {
        // Issue 1 fix: validate every custom command against the safety allowlist
        // before executing. Malicious repos could supply shell-injection payloads
        // via `.harness/config.toml` validation.pre_push.
        for cmd in custom_cmds {
            if let Err(e) = crate::post_validator::validate_command_safety(cmd) {
                return Err(format!("test gate: command rejected by safety check: {e}"));
            }
        }
        custom_cmds.to_vec()
    } else {
        match lang_detect::primary_test_command(project_root) {
            Some(cmd) => vec![cmd],
            None => {
                tracing::info!(
                    project = %project_root.display(),
                    "test gate: no test command detected for project, skipping"
                );
                return Ok(());
            }
        }
    };

    let env: Vec<(&str, &str)> = extra_env
        .iter()
        .map(|(key, value)| (key.as_str(), value.as_str()))
        .collect();

    for cmd in &cmds {
        tracing::info!(cmd = %cmd, "test gate: running tests before accepting LGTM");

        let outcome = validation_executor
            .run(ValidationRequest {
                command: cmd,
                cwd: project_root,
                env: &env,
                timeout: Duration::from_secs(timeout_secs),
            })
            .await;

        match outcome.kind {
            ValidationOutcomeKind::Success => {
                tracing::info!(cmd = %cmd, "test gate: tests passed");
            }
            ValidationOutcomeKind::NonZeroExit { code } => {
                return Err(format!(
                    "Test gate failed (exit {code})\nstdout:\n{stdout}\nstderr:\n{stderr}",
                    stdout = outcome.stdout,
                    stderr = outcome.stderr,
                ));
            }
            ValidationOutcomeKind::SpawnFailed { reason } => {
                return Err(format!("test gate: failed to spawn `{cmd}`: {reason}"));
            }
            ValidationOutcomeKind::WaitFailed { reason } => {
                return Err(format!("test gate: `{cmd}` failed to wait: {reason}"));
            }
            ValidationOutcomeKind::TimedOut => {
                return Err(format!(
                    "Test gate timed out after {timeout_secs}s (command: `{cmd}`)"
                ));
            }
        }
    }

    Ok(())
}
