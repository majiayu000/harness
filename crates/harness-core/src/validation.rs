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
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;
use tokio::time::timeout;

/// Per-stream cap on captured validation output. Verbose validators (e.g.
/// `cargo test -- --nocapture`, `npm test`) can produce hundreds of MiB of
/// output that the caller never reads in full — only error excerpts and exit
/// codes. Bounding each stream prevents an unbounded `wait_with_output()`
/// buffer from inflating harness memory or OOM-killing the server.
///
/// 256 KiB is enough to retain the head of typical build errors and is small
/// enough that even pathological 1 GiB log streams cost at most ~512 KiB
/// resident per concurrent validation.
pub const MAX_CAPTURED_OUTPUT_BYTES: usize = 256 * 1024;

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

        let mut child = match command.spawn() {
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

        // Take stdout/stderr handles before awaiting the process so we can
        // drain them concurrently with `wait()`. `wait_with_output()` would
        // buffer both streams in full, which a verbose validator can grow
        // without bound; reading via `drain_capped` keeps each stream's
        // captured bytes at MAX_CAPTURED_OUTPUT_BYTES while still draining
        // the pipe so the child can keep making progress.
        let stdout_pipe = child.stdout.take();
        let stderr_pipe = child.stderr.take();

        let drain = async {
            let stdout_fut = async {
                if let Some(pipe) = stdout_pipe {
                    drain_capped(pipe, MAX_CAPTURED_OUTPUT_BYTES).await
                } else {
                    Ok(Vec::new())
                }
            };
            let stderr_fut = async {
                if let Some(pipe) = stderr_pipe {
                    drain_capped(pipe, MAX_CAPTURED_OUTPUT_BYTES).await
                } else {
                    Ok(Vec::new())
                }
            };
            let wait_fut = child.wait();
            let (out, err, status) = tokio::join!(stdout_fut, stderr_fut, wait_fut);
            (out, err, status)
        };

        let result = timeout(request.timeout, drain).await;
        match result {
            Ok((Ok(stdout_bytes), Ok(stderr_bytes), Ok(status))) => {
                let stdout = String::from_utf8_lossy(&stdout_bytes).into_owned();
                let stderr = String::from_utf8_lossy(&stderr_bytes).into_owned();
                let kind = if status.success() {
                    ValidationOutcomeKind::Success
                } else {
                    ValidationOutcomeKind::NonZeroExit {
                        code: status.code().unwrap_or(-1),
                    }
                };
                ValidationOutcome {
                    kind,
                    stdout,
                    stderr,
                }
            }
            Ok((_, _, Err(e))) | Ok((Err(e), _, _)) | Ok((_, Err(e), _)) => ValidationOutcome {
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

/// Read from `reader` until EOF, retaining at most `cap` bytes. Bytes past the
/// cap are still consumed (so the child's pipe doesn't fill and block) but
/// discarded. Returns the captured prefix.
async fn drain_capped<R: AsyncRead + Unpin>(mut reader: R, cap: usize) -> std::io::Result<Vec<u8>> {
    let mut captured = Vec::with_capacity(cap.min(8 * 1024));
    let mut chunk = [0u8; 8 * 1024];
    loop {
        let n = reader.read(&mut chunk).await?;
        if n == 0 {
            break;
        }
        if captured.len() < cap {
            let remaining = cap - captured.len();
            captured.extend_from_slice(&chunk[..n.min(remaining)]);
        }
    }
    Ok(captured)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn test_cwd() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    }

    #[tokio::test]
    async fn shell_executor_reports_success_on_zero_exit() {
        let executor = ShellValidationExecutor::new();
        let outcome = executor
            .run(ValidationRequest {
                command: "true",
                cwd: &test_cwd(),
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
                cwd: &test_cwd(),
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
                cwd: &test_cwd(),
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
                cwd: &test_cwd(),
                env: &[],
                timeout: Duration::from_millis(150),
            })
            .await;
        assert_eq!(outcome.kind, ValidationOutcomeKind::TimedOut);
    }

    /// Regression for #1068: a validator that emits more than
    /// `MAX_CAPTURED_OUTPUT_BYTES` bytes should not cause unbounded buffering
    /// or hang. The captured prefix must be exactly `cap` bytes; the child
    /// must still exit cleanly because the drain consumes the remainder.
    #[tokio::test]
    async fn shell_executor_caps_large_stdout_capture() {
        // dd produces ~1 MiB of zero-filled output; we expect captured stdout
        // length to be exactly MAX_CAPTURED_OUTPUT_BYTES (256 KiB), not the
        // full megabyte.
        let executor = ShellValidationExecutor::new();
        let outcome = executor
            .run(ValidationRequest {
                command: "dd if=/dev/zero bs=4096 count=256 2>/dev/null",
                cwd: &test_cwd(),
                env: &[],
                timeout: Duration::from_secs(10),
            })
            .await;
        assert_eq!(outcome.kind, ValidationOutcomeKind::Success);
        assert_eq!(
            outcome.stdout.len(),
            MAX_CAPTURED_OUTPUT_BYTES,
            "stdout capture must be capped at MAX_CAPTURED_OUTPUT_BYTES; \
             got {} bytes (total emitted: ~1 MiB)",
            outcome.stdout.len()
        );
    }

    #[tokio::test]
    async fn shell_executor_propagates_environment() {
        let executor = ShellValidationExecutor::new();
        let outcome = executor
            .run(ValidationRequest {
                command: "printf %s \"$HARNESS_TEST_VAR\"",
                cwd: &test_cwd(),
                env: &[("HARNESS_TEST_VAR", "abc")],
                timeout: Duration::from_secs(5),
            })
            .await;
        assert_eq!(outcome.kind, ValidationOutcomeKind::Success);
        assert_eq!(outcome.stdout, "abc");
    }
}
