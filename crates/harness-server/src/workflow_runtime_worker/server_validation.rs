//! Server-side re-execution of quality-gate validation commands (GH-1766,
//! B-003/B-004).
//!
//! The runtime worker executes the configured validation commands itself in
//! the leased workspace after the agent turn and records a digest (command,
//! working directory, exit code, output hash, duration) as the gate's
//! authoritative evidence. An agent `QualityPassed` claim without a matching
//! server digest never satisfies the gate.
//!
//! Commands come from `WORKFLOW.md` (repo-owned configuration) and are
//! executed without a shell. Structured argv is authoritative; legacy command
//! strings are parsed with shell quoting rules for compatibility.

use harness_workflow::runtime::completion_evidence::ARTIFACT_SERVER_VALIDATION_DIGEST;
use harness_workflow::runtime::{
    ActivityArtifact, ActivityErrorKind, ActivityResult, ActivityStatus,
};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::path::Path;
use std::time::{Duration, Instant};

const MAX_CAPTURED_OUTPUT_BYTES: usize = 64 * 1024;

/// Outcome of the server-side validation run, ready to be folded into the
/// activity result.
pub(super) struct ServerValidationRun {
    pub digest: Value,
    pub failure: Option<ServerValidationFailure>,
}

pub(super) struct ServerValidationFailure {
    pub error: String,
    pub error_kind: ActivityErrorKind,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ValidationCommandSpec {
    display: String,
    argv: Vec<String>,
}

impl ValidationCommandSpec {
    pub(super) fn from_argv(argv: Vec<String>) -> Result<Self, String> {
        if argv.is_empty() {
            return Err("validation command argv must not be empty".to_string());
        }
        if argv[0].is_empty() {
            return Err("validation command program must not be empty".to_string());
        }
        let display = shlex::try_join(argv.iter().map(String::as_str))
            .map_err(|error| format!("validation command argv cannot be represented: {error}"))?;
        Ok(Self { display, argv })
    }

    pub(super) fn from_legacy_string(command: &str) -> Result<Self, String> {
        let command = command.trim();
        let argv = shlex::split(command)
            .ok_or_else(|| format!("validation command has invalid quoting: {command}"))?;
        let mut spec = Self::from_argv(argv)?;
        spec.display = command.to_string();
        Ok(spec)
    }

    fn program(&self) -> &str {
        &self.argv[0]
    }

    fn args(&self) -> &[String] {
        &self.argv[1..]
    }
}

/// Execute `commands` sequentially in `workspace_root`, stopping at the
/// first failure. Every started command records an entry in the digest.
pub(super) async fn run_validation_commands(
    workspace_root: &Path,
    commands: &[ValidationCommandSpec],
    timeout: Duration,
    credential_environment: Option<&crate::eval_credentials::EvalCredentialEnvironment>,
) -> ServerValidationRun {
    if commands.is_empty() {
        let mut digest = json!({
            "commands": [],
            "cwd": workspace_root.display().to_string(),
            "startup_error": "validation_commands_missing",
        });
        if let Some(credential_environment) = credential_environment {
            digest["credential_environment"] = json!(credential_environment.audit());
        }
        return ServerValidationRun {
            digest,
            failure: Some(ServerValidationFailure {
                error: "validation_commands_missing: the quality gate has no configured \
                        validation commands; an unvalidated gate never passes"
                    .to_string(),
                error_kind: ActivityErrorKind::Configuration,
            }),
        };
    }

    let started = Instant::now();
    let mut entries = Vec::new();
    let mut failure = None;
    for command in commands {
        let remaining = timeout.saturating_sub(started.elapsed());
        let entry =
            run_single_command(workspace_root, command, remaining, credential_environment).await;
        let failed = entry.get("exit_code").and_then(Value::as_i64) != Some(0)
            || entry.get("startup_error").is_some();
        entries.push(entry);
        if failed {
            let last = entries.last().cloned().unwrap_or(Value::Null);
            failure = Some(server_validation_failure_for_entry(&command.display, &last));
            break;
        }
    }

    let mut digest = json!({
        "commands": entries,
        "cwd": workspace_root.display().to_string(),
        "total_duration_ms": started.elapsed().as_millis() as u64,
    });
    if let Some(credential_environment) = credential_environment {
        digest["credential_environment"] = json!(credential_environment.audit());
    }
    ServerValidationRun { digest, failure }
}

fn server_validation_failure_for_entry(command: &str, entry: &Value) -> ServerValidationFailure {
    if let Some(startup_error) = entry.get("startup_error").and_then(Value::as_str) {
        let error_kind = if startup_error.contains("timed out") {
            ActivityErrorKind::Timeout
        } else {
            ActivityErrorKind::Retryable
        };
        return ServerValidationFailure {
            error: format!("server validation `{command}` did not run: {startup_error}"),
            error_kind,
        };
    }
    let exit_code = entry.get("exit_code").and_then(Value::as_i64);
    ServerValidationFailure {
        error: format!(
            "server validation `{command}` failed with exit code {}",
            exit_code.map_or("unknown".to_string(), |code| code.to_string())
        ),
        error_kind: ActivityErrorKind::Fatal,
    }
}

async fn run_single_command(
    workspace_root: &Path,
    command: &ValidationCommandSpec,
    timeout: Duration,
    credential_environment: Option<&crate::eval_credentials::EvalCredentialEnvironment>,
) -> Value {
    let started = Instant::now();
    let mut process = tokio::process::Command::new(command.program());
    process
        .args(command.args())
        .current_dir(workspace_root)
        .kill_on_drop(true)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    if let Some(credential_environment) = credential_environment {
        process.env_clear().envs(credential_environment.variables());
    }
    let output = match tokio::time::timeout(timeout, process.output()).await {
        Ok(Ok(output)) => output,
        Ok(Err(error)) => {
            return command_entry(
                command,
                workspace_root,
                None,
                "",
                started.elapsed().as_millis() as u64,
                Some(&format!("spawn failed: {error}")),
            );
        }
        Err(_) => {
            return command_entry(
                command,
                workspace_root,
                None,
                "",
                started.elapsed().as_millis() as u64,
                Some(&format!("timed out after {}s", timeout.as_secs())),
            );
        }
    };
    let mut combined = output.stdout;
    combined.extend_from_slice(&output.stderr);
    let truncated = combined.len() > MAX_CAPTURED_OUTPUT_BYTES;
    let output_sha256 = format!("{:x}", Sha256::digest(&combined));
    let mut entry = command_entry(
        command,
        workspace_root,
        output.status.code().map(i64::from),
        &output_sha256,
        started.elapsed().as_millis() as u64,
        None,
    );
    entry["truncated"] = json!(truncated);
    if output.status.code() != Some(0) {
        combined.truncate(MAX_CAPTURED_OUTPUT_BYTES.min(4096));
        entry["output_tail"] = json!(String::from_utf8_lossy(&combined).to_string());
    }
    entry
}

fn command_entry(
    command: &ValidationCommandSpec,
    cwd: &Path,
    exit_code: Option<i64>,
    output_sha256: &str,
    duration_ms: u64,
    startup_error: Option<&str>,
) -> Value {
    let mut entry = json!({
        "command": command.display,
        "argv": command.argv,
        "cwd": cwd.display().to_string(),
        "duration_ms": duration_ms,
    });
    if let Some(exit_code) = exit_code {
        entry["exit_code"] = json!(exit_code);
    }
    if !output_sha256.is_empty() {
        entry["output_sha256"] = json!(output_sha256);
    }
    if let Some(startup_error) = startup_error {
        entry["startup_error"] = json!(startup_error);
    }
    entry
}

/// Fold a completed server validation run into the agent's activity result.
///
/// All commands passing attaches the digest and leaves the result intact.
/// Any failure attaches the digest and rewrites the result as a failed
/// activity: the server run is authoritative over the agent's claim.
pub(super) fn apply_server_validation(
    mut result: ActivityResult,
    run: ServerValidationRun,
) -> ActivityResult {
    result = result.with_artifact(ActivityArtifact::new(
        ARTIFACT_SERVER_VALIDATION_DIGEST,
        run.digest,
    ));
    let Some(failure) = run.failure else {
        return result;
    };
    result.status = ActivityStatus::Failed;
    result.summary = format!(
        "Server-side quality gate validation failed: {} (agent-reported outcome superseded).",
        failure.error
    );
    result.error = Some(failure.error);
    result.error_kind = Some(failure.error_kind);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_workflow::runtime::completion_evidence::{
        server_validation_digest_artifact, server_validation_digest_passed,
    };

    fn workspace() -> tempfile::TempDir {
        tempfile::tempdir().expect("tempdir")
    }

    fn command_spec(command: &str) -> ValidationCommandSpec {
        ValidationCommandSpec::from_legacy_string(command).expect("valid command")
    }

    #[tokio::test]
    async fn passing_commands_record_digest_and_keep_result() {
        let dir = workspace();
        let run = run_validation_commands(
            dir.path(),
            &[command_spec("true"), command_spec("true")],
            Duration::from_secs(30),
            None,
        )
        .await;
        assert!(run.failure.is_none());
        let result =
            apply_server_validation(ActivityResult::succeeded("run_quality_gate", "ok"), run);
        assert_eq!(result.status, ActivityStatus::Succeeded);
        assert!(server_validation_digest_passed(&result));
        let digest = server_validation_digest_artifact(&result).expect("digest attached");
        assert_eq!(digest["commands"].as_array().map(Vec::len), Some(2));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn command_environment_strips_secrets_and_keeps_allowlisted_plain_values(
    ) -> anyhow::Result<()> {
        use std::collections::HashMap;
        use std::os::unix::fs::PermissionsExt;

        let dir = workspace();
        let script = dir.path().join("assert-secretless-env");
        std::fs::write(
            &script,
            r#"#!/bin/sh
test -z "${OPENAI_API_KEY+x}" || exit 42
test -z "${GITHUB_TOKEN+x}" || exit 43
test "$SAFE_FLAG" = "ok" || exit 44
test -n "$PATH" || exit 45
"#,
        )?;
        let mut permissions = std::fs::metadata(&script)?.permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&script, permissions)?;

        let ambient_env = HashMap::from([
            ("PATH".to_string(), "/bin:/usr/bin".to_string()),
            ("SAFE_FLAG".to_string(), "ok".to_string()),
            ("OPENAI_API_KEY".to_string(), "sk-secret".to_string()),
            ("GITHUB_TOKEN".to_string(), "ghp-secret".to_string()),
        ]);
        let plain_env_allowlist = ["PATH", "SAFE_FLAG", "OPENAI_API_KEY", "GITHUB_TOKEN"]
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>();
        let credential_environment = crate::eval_credentials::build_eval_credential_environment(
            &ambient_env,
            &plain_env_allowlist,
            &[],
            &[],
            chrono::Utc::now(),
        )?;
        let command = script.to_str().expect("temp path is utf-8");
        assert!(
            !command.chars().any(char::is_whitespace),
            "test command path must not contain whitespace"
        );

        let entry = run_single_command(
            dir.path(),
            &command_spec(command),
            Duration::from_secs(30),
            Some(&credential_environment),
        )
        .await;

        assert_eq!(entry["exit_code"], 0);
        assert_eq!(credential_environment.variables()["SAFE_FLAG"], "ok");
        assert!(!credential_environment
            .variables()
            .contains_key("OPENAI_API_KEY"));
        assert!(!credential_environment
            .variables()
            .contains_key("GITHUB_TOKEN"));
        assert_eq!(
            credential_environment
                .audit()
                .stripped_env
                .iter()
                .map(|item| (item.key.as_str(), item.class))
                .collect::<Vec<_>>(),
            [
                (
                    "GITHUB_TOKEN",
                    crate::eval_credentials::EvalSecretEnvClass::GitHub
                ),
                (
                    "OPENAI_API_KEY",
                    crate::eval_credentials::EvalSecretEnvClass::Provider
                ),
            ]
        );
        Ok(())
    }

    #[tokio::test]
    async fn failing_command_supersedes_agent_success() {
        let dir = workspace();
        let run = run_validation_commands(
            dir.path(),
            &[command_spec("false"), command_spec("true")],
            Duration::from_secs(30),
            None,
        )
        .await;
        assert!(run.failure.is_some());
        let result = apply_server_validation(
            ActivityResult::succeeded("run_quality_gate", "agent claims pass"),
            run,
        );
        assert_eq!(result.status, ActivityStatus::Failed);
        assert_eq!(result.error_kind, Some(ActivityErrorKind::Fatal));
        assert!(!server_validation_digest_passed(&result));
        // fail-fast: the second command never ran
        let digest = server_validation_digest_artifact(&result).expect("digest attached");
        assert_eq!(digest["commands"].as_array().map(Vec::len), Some(1));
    }

    #[tokio::test]
    async fn missing_commands_never_pass() {
        let dir = workspace();
        let run = run_validation_commands(dir.path(), &[], Duration::from_secs(5), None).await;
        let failure = run.failure.as_ref().expect("missing commands must fail");
        assert!(failure.error.contains("validation_commands_missing"));
        let result = apply_server_validation(
            ActivityResult::succeeded("run_quality_gate", "agent claims pass"),
            run,
        );
        assert_eq!(result.status, ActivityStatus::Failed);
        assert_eq!(result.error_kind, Some(ActivityErrorKind::Configuration));
        assert!(!server_validation_digest_passed(&result));
    }

    #[tokio::test]
    async fn spawn_failure_records_startup_error() {
        let dir = workspace();
        let run = run_validation_commands(
            dir.path(),
            &[command_spec("definitely-not-a-real-binary-gh1766")],
            Duration::from_secs(5),
            None,
        )
        .await;
        let failure = run.failure.as_ref().expect("spawn failure must fail");
        assert_eq!(
            std::mem::discriminant(&failure.error_kind),
            std::mem::discriminant(&ActivityErrorKind::Retryable)
        );
        assert!(run.digest["commands"][0]["startup_error"]
            .as_str()
            .unwrap_or_default()
            .contains("spawn failed"));
    }

    #[tokio::test]
    async fn timeout_records_timeout_error_kind() {
        let dir = workspace();
        let run = run_validation_commands(
            dir.path(),
            &[command_spec("sleep 5")],
            Duration::from_millis(100),
            None,
        )
        .await;
        let failure = run.failure.as_ref().expect("timeout must fail");
        assert_eq!(
            std::mem::discriminant(&failure.error_kind),
            std::mem::discriminant(&ActivityErrorKind::Timeout)
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn timeout_terminates_the_spawned_validation_process() {
        let dir = workspace();
        let marker = dir.path().join("leaked-process-marker");
        let command = ValidationCommandSpec::from_argv(vec![
            "sh".to_string(),
            "-c".to_string(),
            "sleep 1; touch \"$1\"".to_string(),
            "validation-timeout-test".to_string(),
            marker.display().to_string(),
        ])
        .expect("valid command");

        let run =
            run_validation_commands(dir.path(), &[command], Duration::from_millis(50), None).await;
        assert!(run.failure.is_some());
        tokio::time::sleep(Duration::from_millis(1_200)).await;
        assert!(
            !marker.exists(),
            "timed-out validation process must be killed"
        );
    }
}
