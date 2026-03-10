use async_trait::async_trait;
use harness_core::{
    interceptor::{InterceptResult, PostExecuteResult, TurnInterceptor},
    lang_detect, AgentRequest, AgentResponse, ValidationConfig,
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

impl PostExecutionValidator {
    pub fn new(config: ValidationConfig) -> Self {
        Self { config }
    }

    /// Reject commands that pipe output to a shell interpreter, the most common
    /// code-execution escalation vector.
    ///
    /// Legitimate CI commands (cargo, go, npm, ruff, …) never need to feed their
    /// output into another shell.  Commands that do — e.g. `cmd | bash` or
    /// `cmd | sh -` — indicate either an attacker-supplied payload or a
    /// misconfigured `ValidationConfig`.
    ///
    /// Note: the check is a fast prefix/substring scan, not a full shell parser.
    /// It is defence-in-depth against the most dangerous class of injection, not a
    /// complete sandbox.
    fn reject_shell_pipe(cmd_str: &str) -> Result<(), String> {
        const DANGEROUS: &[&str] = &[
            "| bash", "|bash", "| sh ", "|sh ", "| sh\t", "|sh\t", "| zsh", "|zsh", "| fish",
            "|fish", "| python", "|python", "| perl", "|perl", "| ruby", "|ruby", "; bash ",
            ";bash ", "; sh ", ";sh ",
        ];
        if let Some(pat) = DANGEROUS.iter().find(|&&p| cmd_str.contains(p)) {
            return Err(format!(
                "Command `{cmd_str}` rejected: contains unsafe shell pipe pattern `{pat}`. \
                 Validation commands must invoke toolchain binaries directly."
            ));
        }
        Ok(())
    }

    /// Run a shell command via `sh -c` to support pipes, quotes, and complex expressions.
    ///
    /// # Security
    ///
    /// `cmd_str` MUST come from a trusted source — either the server's own startup
    /// `ValidationConfig` (loaded from the operator-controlled config file) or the
    /// hardcoded language defaults in `harness_core::lang_detect`.  Never pass a string
    /// derived from a checked-out worktree or any user-supplied request body; doing so
    /// would allow arbitrary code execution.
    ///
    /// Uses `kill_on_drop(true)` so that if the timeout fires and the future is dropped,
    /// the child process is killed rather than left running as an orphan.
    async fn run_command(cmd_str: &str, project: &Path, timeout_secs: u64) -> Result<(), String> {
        if cmd_str.trim().is_empty() {
            return Ok(());
        }

        Self::reject_shell_pipe(cmd_str)?;

        let child = match Command::new("sh")
            .args(["-c", cmd_str])
            .current_dir(project)
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
        let lang = lang_detect::detect_language(project);

        // Resolve pre_commit commands: config takes precedence over language detection.
        let pre_commit = if self.config.pre_commit.is_empty() {
            lang_detect::default_pre_commit_commands(lang, project)
        } else {
            self.config.pre_commit.clone()
        };

        // Resolve pre_push commands: config takes precedence over language detection.
        let pre_push = if self.config.pre_push.is_empty() {
            lang_detect::default_pre_push_commands(lang, project)
        } else {
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

    // ── reject_shell_pipe ─────────────────────────────────────────────────────

    #[test]
    fn safe_commands_pass_shell_pipe_check() {
        let safe = [
            "cargo check --workspace",
            "go test ./...",
            "npx tsc --noEmit",
            "test -z \"$(gofmt -l .)\"",
            "ruff check .",
            "./gradlew check",
            "bundle exec rubocop",
            "true",
            "false",
        ];
        for cmd in safe {
            assert!(
                PostExecutionValidator::reject_shell_pipe(cmd).is_ok(),
                "expected `{cmd}` to pass but it was rejected"
            );
        }
    }

    #[test]
    fn dangerous_commands_are_rejected() {
        let dangerous = [
            "echo foo | bash",
            "echo foo |bash",
            "echo foo | sh -",
            "echo foo |sh ",
            "something ; bash -c evil",
            "something ;bash -c evil",
            "cmd | zsh",
            "cmd | python3 -",
            "cmd | perl -e 'code'",
            "cmd | ruby -e 'code'",
        ];
        for cmd in dangerous {
            assert!(
                PostExecutionValidator::reject_shell_pipe(cmd).is_err(),
                "expected `{cmd}` to be rejected but it passed"
            );
        }
    }
}
