use harness_core::{config::agents::CodexCloudConfig, error::HarnessError};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};
use tokio::process::Command;

const SETUP_OUTPUT_MAX_BYTES: usize = 512;
pub(crate) const SETUP_ENV_ALLOWLIST: [&str; 10] = [
    "PATH", "HOME", "USER", "SHELL", "TMPDIR", "TMP", "TEMP", "LANG", "LC_ALL", "LC_CTYPE",
];

/// Reject setup commands that contain shell operators enabling injection.
///
/// `setup_commands` must only come from server-level config, never from
/// per-project `.harness/config.toml`, and must invoke a single binary
/// without piping or chaining (e.g. `npm ci`, `cargo fetch`).
///
/// Redirections (`>`, `>>`, `<`) are permitted because setup tasks
/// commonly suppress output (e.g. `npm ci > /dev/null`). Command
/// chaining/backgrounding operators are always rejected.
///
/// Delegates to [`harness_core::shell_safety::validate_shell_safety`] with
/// `allow_redirections = true`.
fn validate_setup_command(cmd: &str) -> Result<(), String> {
    harness_core::shell_safety::validate_shell_safety(cmd, true)
        .map_err(|e| e.replace("Command `", "setup command `"))
}

fn setup_cache_ttl(cloud: &CodexCloudConfig) -> Duration {
    Duration::from_secs(cloud.cache_ttl_hours.saturating_mul(3600))
}

pub(crate) fn setup_cache_key(cloud: &CodexCloudConfig, project_root: &Path) -> String {
    let fingerprint = serde_json::json!({
        "project_root": project_root.to_string_lossy(),
        "setup_commands": cloud.setup_commands,
        "setup_secret_env": cloud.setup_secret_env,
    })
    .to_string();

    let mut hasher = Sha256::new();
    hasher.update(fingerprint.as_bytes());
    let digest = hasher.finalize();

    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn setup_cache_stamp_path(cloud: &CodexCloudConfig, project_root: &Path) -> PathBuf {
    let key = setup_cache_key(cloud, project_root);
    project_root
        .join(".harness")
        .join("cloud-setup-cache")
        .join(format!("{key}.stamp"))
}

fn setup_cache_is_fresh(
    cloud: &CodexCloudConfig,
    project_root: &Path,
) -> harness_core::error::Result<bool> {
    if cloud.cache_ttl_hours == 0 {
        return Ok(false);
    }

    let stamp = setup_cache_stamp_path(cloud, project_root);
    let metadata = match fs::metadata(&stamp) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(err) => {
            return Err(HarnessError::AgentExecution(format!(
                "failed to read cloud setup cache metadata `{}`: {err}",
                stamp.display()
            )));
        }
    };

    let modified = metadata.modified().map_err(|err| {
        HarnessError::AgentExecution(format!(
            "failed to read cloud setup cache mtime `{}`: {err}",
            stamp.display()
        ))
    })?;

    let age = SystemTime::now()
        .duration_since(modified)
        .unwrap_or(Duration::ZERO);

    Ok(age <= setup_cache_ttl(cloud))
}

fn write_setup_cache_stamp(
    cloud: &CodexCloudConfig,
    project_root: &Path,
) -> harness_core::error::Result<()> {
    if cloud.cache_ttl_hours == 0 {
        return Ok(());
    }

    let stamp = setup_cache_stamp_path(cloud, project_root);
    let Some(parent) = stamp.parent() else {
        return Err(HarnessError::AgentExecution(format!(
            "invalid cloud setup cache path `{}`",
            stamp.display()
        )));
    };

    fs::create_dir_all(parent).map_err(|err| {
        HarnessError::AgentExecution(format!(
            "failed to create cloud setup cache dir `{}`: {err}",
            parent.display()
        ))
    })?;

    fs::write(&stamp, b"ok\n").map_err(|err| {
        HarnessError::AgentExecution(format!(
            "failed to write cloud setup cache stamp `{}`: {err}",
            stamp.display()
        ))
    })?;

    Ok(())
}

fn apply_setup_environment(cmd: &mut Command, cloud: &CodexCloudConfig) {
    cmd.env_clear();

    for key in SETUP_ENV_ALLOWLIST {
        if let Ok(value) = std::env::var(key) {
            cmd.env(key, value);
        }
    }

    for key in &cloud.setup_secret_env {
        if let Ok(value) = std::env::var(key) {
            cmd.env(key, value);
        }
    }
}

pub(crate) async fn run_setup_phase(
    cloud: &CodexCloudConfig,
    project_root: &Path,
) -> harness_core::error::Result<()> {
    if !cloud.enabled || cloud.setup_commands.is_empty() {
        return Ok(());
    }

    if setup_cache_is_fresh(cloud, project_root)? {
        return Ok(());
    }

    for setup_command in &cloud.setup_commands {
        if setup_command.trim().is_empty() {
            continue;
        }

        if let Err(msg) = validate_setup_command(setup_command) {
            return Err(HarnessError::AgentExecution(msg));
        }

        let mut cmd = Command::new("sh");
        cmd.arg("-lc").arg(setup_command).current_dir(project_root);
        apply_setup_environment(&mut cmd, cloud);

        let output = cmd.output().await.map_err(|err| {
            HarnessError::AgentExecution(format!(
                "failed to run cloud setup command `{setup_command}`: {err}"
            ))
        })?;

        if !output.status.success() {
            let detail = command_output_summary(&output, &cloud.setup_secret_env);
            return Err(HarnessError::AgentExecution(format!(
                "cloud setup command `{setup_command}` failed with {}: {detail}",
                output.status
            )));
        }
    }

    write_setup_cache_stamp(cloud, project_root)?;
    Ok(())
}

fn redact_secret_values(mut text: String, secret_values: &[String]) -> String {
    for secret_value in secret_values {
        if !secret_value.is_empty() {
            text = text.replace(secret_value, "***");
        }
    }
    text
}

fn truncate_to_max_bytes(mut text: String, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text;
    }

    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    text
}

pub(crate) fn command_output_summary(
    output: &std::process::Output,
    secret_env: &[String],
) -> String {
    let secret_values: Vec<String> = secret_env
        .iter()
        .filter_map(|key| std::env::var(key).ok())
        .filter(|value| !value.is_empty())
        .collect();
    command_output_summary_with_secret_values(output, &secret_values)
}

fn command_output_summary_with_secret_values(
    output: &std::process::Output,
    secret_values: &[String],
) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let summary = if !stderr.is_empty() {
        stderr
    } else if !stdout.is_empty() {
        stdout
    } else {
        "no output".to_string()
    };

    let redacted = redact_secret_values(summary, secret_values);
    truncate_to_max_bytes(redacted, SETUP_OUTPUT_MAX_BYTES)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Command as StdCommand, Output};

    fn successful_status() -> std::process::ExitStatus {
        StdCommand::new("sh")
            .arg("-lc")
            .arg("exit 0")
            .output()
            .unwrap_or_else(|e| panic!("status command should run: {e}"))
            .status
    }

    #[test]
    fn setup_cache_key_is_deterministic() {
        let cloud = CodexCloudConfig {
            enabled: true,
            cache_ttl_hours: 12,
            setup_commands: vec!["npm ci".to_string()],
            setup_secret_env: vec!["NPM_TOKEN".to_string()],
        };
        let project_root = Path::new("/tmp/project");

        let first = setup_cache_key(&cloud, project_root);
        let second = setup_cache_key(&cloud, project_root);

        assert_eq!(first, second);
    }

    #[test]
    fn setup_cache_key_changes_when_setup_changes() {
        let project_root = Path::new("/tmp/project");
        let first = CodexCloudConfig {
            enabled: true,
            cache_ttl_hours: 12,
            setup_commands: vec!["npm ci".to_string()],
            setup_secret_env: vec!["NPM_TOKEN".to_string()],
        };
        let second = CodexCloudConfig {
            enabled: true,
            cache_ttl_hours: 12,
            setup_commands: vec!["cargo fetch".to_string()],
            setup_secret_env: vec!["NPM_TOKEN".to_string()],
        };

        assert_ne!(
            setup_cache_key(&first, project_root),
            setup_cache_key(&second, project_root)
        );
    }

    #[test]
    fn setup_cache_key_ignores_ttl_hours() {
        let project_root = Path::new("/tmp/project");
        let first = CodexCloudConfig {
            enabled: true,
            cache_ttl_hours: 1,
            setup_commands: vec!["npm ci".to_string()],
            setup_secret_env: vec!["NPM_TOKEN".to_string()],
        };
        let second = CodexCloudConfig {
            enabled: true,
            cache_ttl_hours: 24,
            setup_commands: vec!["npm ci".to_string()],
            setup_secret_env: vec!["NPM_TOKEN".to_string()],
        };

        assert_eq!(
            setup_cache_key(&first, project_root),
            setup_cache_key(&second, project_root)
        );
    }

    #[test]
    fn command_output_summary_redacts_configured_secrets() {
        let secret_value = "secret-token-value";
        let output = Output {
            status: successful_status(),
            stdout: Vec::new(),
            stderr: format!("failed with token={secret_value}").into_bytes(),
        };

        let summary =
            command_output_summary_with_secret_values(&output, &[secret_value.to_string()]);

        assert!(!summary.contains(secret_value));
        assert!(summary.contains("***"));
    }

    #[test]
    fn command_output_summary_truncates_to_512_bytes() {
        let output = Output {
            status: successful_status(),
            stdout: Vec::new(),
            stderr: "x".repeat(2048).into_bytes(),
        };

        let summary = command_output_summary(&output, &[]);
        assert_eq!(summary.len(), 512);
    }

    #[test]
    fn validate_setup_command_accepts_simple_command() {
        assert!(validate_setup_command("npm ci").is_ok());
        assert!(validate_setup_command("cargo fetch").is_ok());
        assert!(validate_setup_command("pip install -r requirements.txt").is_ok());
    }

    #[test]
    fn validate_setup_command_accepts_output_redirection() {
        assert!(validate_setup_command("npm ci > /dev/null").is_ok());
        assert!(validate_setup_command("cargo fetch 2>/dev/null").is_ok());
    }

    #[test]
    fn validate_setup_command_rejects_command_chaining() {
        assert!(validate_setup_command("npm ci && rm -rf /").is_err());
        assert!(validate_setup_command("npm ci; echo pwned").is_err());
        assert!(validate_setup_command("npm ci || echo fallback").is_err());
    }

    #[test]
    fn validate_setup_command_rejects_background_execution() {
        assert!(validate_setup_command("npm ci &").is_err());
    }

    #[test]
    fn setup_cache_key_changes_when_project_root_changes() {
        let cloud = CodexCloudConfig {
            enabled: true,
            cache_ttl_hours: 12,
            setup_commands: vec!["npm ci".to_string()],
            setup_secret_env: vec!["NPM_TOKEN".to_string()],
        };

        let key_a = setup_cache_key(&cloud, Path::new("/tmp/project-a"));
        let key_b = setup_cache_key(&cloud, Path::new("/tmp/project-b"));

        assert_ne!(key_a, key_b);
    }

    #[test]
    fn setup_cache_key_changes_when_secret_env_changes() {
        let project_root = Path::new("/tmp/project");
        let with_token = CodexCloudConfig {
            enabled: true,
            cache_ttl_hours: 12,
            setup_commands: vec!["npm ci".to_string()],
            setup_secret_env: vec!["NPM_TOKEN".to_string()],
        };
        let without_token = CodexCloudConfig {
            enabled: true,
            cache_ttl_hours: 12,
            setup_commands: vec!["npm ci".to_string()],
            setup_secret_env: Vec::new(),
        };

        assert_ne!(
            setup_cache_key(&with_token, project_root),
            setup_cache_key(&without_token, project_root)
        );
    }

    #[tokio::test]
    async fn run_setup_phase_noop_when_cloud_disabled() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let marker = dir.path().join("should-not-exist.txt");
        let setup = format!("touch \"{}\"", marker.display());

        let cloud = CodexCloudConfig {
            enabled: false,
            cache_ttl_hours: 12,
            setup_commands: vec![setup],
            setup_secret_env: Vec::new(),
        };

        run_setup_phase(&cloud, dir.path()).await?;

        assert!(!marker.exists(), "setup command must not run when disabled");
        Ok(())
    }

    #[tokio::test]
    async fn run_setup_phase_noop_when_no_commands() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let cloud = CodexCloudConfig {
            enabled: true,
            cache_ttl_hours: 12,
            setup_commands: Vec::new(),
            setup_secret_env: Vec::new(),
        };

        run_setup_phase(&cloud, dir.path()).await?;
        Ok(())
    }

    #[tokio::test]
    async fn run_setup_phase_rejects_chaining_command() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let cloud = CodexCloudConfig {
            enabled: true,
            cache_ttl_hours: 12,
            setup_commands: vec!["npm ci && echo pwned".to_string()],
            setup_secret_env: Vec::new(),
        };

        let result = run_setup_phase(&cloud, dir.path()).await;
        assert!(result.is_err(), "chaining command must be rejected");
        Ok(())
    }
}
