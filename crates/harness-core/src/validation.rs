//! Abstraction for executing user-supplied shell validation commands.
//!
//! Two production sites in `harness-server` (post-execution validation and
//! the pre-merge test gate) historically called `Command::new("sh").args(["-c",
//! cmd]).spawn()` directly, bypassing every trait abstraction. That made them
//! impossible to mock in unit tests and forced anyone wanting to assert
//! merge-gate behaviour to spin up a real shell.
//!
//! This trait gives both sites a single, mockable substitution point.

use async_trait::async_trait;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;
use tokio::process::Command;
use tokio::time::timeout;

/// What the executor is being asked to run.
pub struct ValidationRequest<'a> {
    /// Shell command string. Passed to `sh -c` by the default implementation.
    pub command: &'a str,
    /// Working directory for the spawned process.
    pub cwd: &'a Path,
    /// Environment variables to set on the spawned process. Empty slice means
    /// the executor inherits only the harness process environment.
    pub env: &'a [(&'a str, &'a str)],
    /// Hard timeout. The default implementation kills the child via
    /// `kill_on_drop(true)` if the timeout expires.
    pub timeout: Duration,
}

/// Structured outcome of a validation run. Each caller formats its own
/// user-facing error message because the message wording is part of the
/// caller's UX (e.g. "test gate failed (exit N)" vs "Command \`X\` failed").
#[derive(Debug, Clone)]
pub struct ValidationOutcome {
    pub kind: ValidationOutcomeKind,
    pub stdout: String,
    pub stderr: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValidationOutcomeKind {
    Success,
    NonZeroExit { code: i32 },
    SpawnFailed { reason: String },
    WaitFailed { reason: String },
    TimedOut,
}

impl ValidationOutcome {
    pub fn is_success(&self) -> bool {
        matches!(self.kind, ValidationOutcomeKind::Success)
    }
}

/// Run shell validation commands. Production uses [`ShellValidationExecutor`];
/// tests can substitute a mock to assert how callers interpret outcomes
/// (e.g. that a non-zero exit code blocks the merge gate).
#[async_trait]
pub trait ValidationExecutor: Send + Sync + std::fmt::Debug {
    async fn run<'a>(&self, request: ValidationRequest<'a>) -> ValidationOutcome;
}

/// Production implementation: spawns `sh -c <command>` via Tokio with
/// `kill_on_drop(true)` so the child cannot leak past the timeout.
#[derive(Debug, Default, Clone, Copy)]
pub struct ShellValidationExecutor;

impl ShellValidationExecutor {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl ValidationExecutor for ShellValidationExecutor {
    async fn run<'a>(&self, request: ValidationRequest<'a>) -> ValidationOutcome {
        let mut command = Command::new("sh");
        command
            .args(["-c", request.command])
            .current_dir(request.cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        for (key, value) in request.env {
            command.env(*key, *value);
        }

        let child = match command.spawn() {
            Ok(c) => c,
            Err(e) => {
                return ValidationOutcome {
                    kind: ValidationOutcomeKind::SpawnFailed {
                        reason: e.to_string(),
                    },
                    stdout: String::new(),
                    stderr: String::new(),
                };
            }
        };

        let result = timeout(request.timeout, child.wait_with_output()).await;
        match result {
            Ok(Ok(output)) => {
                let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
                let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
                let kind = if output.status.success() {
                    ValidationOutcomeKind::Success
                } else {
                    ValidationOutcomeKind::NonZeroExit {
                        code: output.status.code().unwrap_or(-1),
                    }
                };
                ValidationOutcome {
                    kind,
                    stdout,
                    stderr,
                }
            }
            Ok(Err(e)) => ValidationOutcome {
                kind: ValidationOutcomeKind::WaitFailed {
                    reason: e.to_string(),
                },
                stdout: String::new(),
                stderr: String::new(),
            },
            Err(_) => ValidationOutcome {
                kind: ValidationOutcomeKind::TimedOut,
                stdout: String::new(),
                stderr: String::new(),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[tokio::test]
    async fn shell_executor_reports_success_on_zero_exit() {
        let executor = ShellValidationExecutor::new();
        let outcome = executor
            .run(ValidationRequest {
                command: "true",
                cwd: &PathBuf::from("."),
                env: &[],
                timeout: Duration::from_secs(5),
            })
            .await;
        assert_eq!(outcome.kind, ValidationOutcomeKind::Success);
        assert!(outcome.is_success());
    }

    #[tokio::test]
    async fn shell_executor_reports_non_zero_exit() {
        let executor = ShellValidationExecutor::new();
        let outcome = executor
            .run(ValidationRequest {
                command: "exit 7",
                cwd: &PathBuf::from("."),
                env: &[],
                timeout: Duration::from_secs(5),
            })
            .await;
        assert_eq!(outcome.kind, ValidationOutcomeKind::NonZeroExit { code: 7 });
        assert!(!outcome.is_success());
    }

    #[tokio::test]
    async fn shell_executor_captures_stdout_and_stderr() {
        let executor = ShellValidationExecutor::new();
        let outcome = executor
            .run(ValidationRequest {
                command: "printf out; printf err >&2",
                cwd: &PathBuf::from("."),
                env: &[],
                timeout: Duration::from_secs(5),
            })
            .await;
        assert_eq!(outcome.kind, ValidationOutcomeKind::Success);
        assert_eq!(outcome.stdout, "out");
        assert_eq!(outcome.stderr, "err");
    }

    #[tokio::test]
    async fn shell_executor_times_out_long_command() {
        let executor = ShellValidationExecutor::new();
        let outcome = executor
            .run(ValidationRequest {
                command: "sleep 5",
                cwd: &PathBuf::from("."),
                env: &[],
                timeout: Duration::from_millis(150),
            })
            .await;
        assert_eq!(outcome.kind, ValidationOutcomeKind::TimedOut);
    }

    #[tokio::test]
    async fn shell_executor_propagates_environment() {
        let executor = ShellValidationExecutor::new();
        let outcome = executor
            .run(ValidationRequest {
                command: "printf %s \"$HARNESS_TEST_VAR\"",
                cwd: &PathBuf::from("."),
                env: &[("HARNESS_TEST_VAR", "abc")],
                timeout: Duration::from_secs(5),
            })
            .await;
        assert_eq!(outcome.kind, ValidationOutcomeKind::Success);
        assert_eq!(outcome.stdout, "abc");
    }
}
