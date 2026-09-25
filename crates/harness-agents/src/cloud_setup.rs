use harness_core::agent::{
    AGENT_CONTAINER_IMAGE_ENV, AGENT_EGRESS_PROXY_IMAGE_ENV, AGENT_ISOLATION_TIER_ENV,
    AGENT_NETWORK_ALLOWLIST_ENV,
};
use harness_core::capability::CapabilityToken;
use harness_core::config::agents::{AgentPermissionMode, CodexCloudConfig, SandboxMode};
use harness_core::error::HarnessError;
use harness_sandbox::SandboxSpec;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, SystemTime};
use tokio::process::Command;

mod container_state;
pub(crate) use container_state::apply_container_state;

const SETUP_OUTPUT_MAX_BYTES: usize = 512;
const SETUP_CAPTURE_MAX_BYTES: usize = 4096;
const SETUP_CACHE_LAYOUT_VERSION: u8 = 2;
pub(crate) const SETUP_ENV_ALLOWLIST: [&str; 10] = [
    "PATH", "HOME", "USER", "SHELL", "TMPDIR", "TMP", "TEMP", "LANG", "LC_ALL", "LC_CTYPE",
];

pub(crate) struct CloudSetupContext<'a> {
    pub(crate) project_root: &'a Path,
    pub(crate) sandbox_mode: SandboxMode,
    pub(crate) permission_mode: AgentPermissionMode,
    pub(crate) env_vars: &'a HashMap<String, String>,
    pub(crate) capability_token: Option<&'a CapabilityToken>,
}

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

fn setup_sandbox_mode(mode: SandboxMode) -> SandboxMode {
    match mode {
        SandboxMode::ReadOnly | SandboxMode::ReadOnlyWithNetwork => SandboxMode::WorkspaceWrite,
        mode => mode,
    }
}

pub(crate) fn setup_cache_key(cloud: &CodexCloudConfig, project_root: &Path) -> String {
    let fingerprint = serde_json::json!({
        "layout_version": SETUP_CACHE_LAYOUT_VERSION,
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

fn setup_spawn_env(
    cloud: &CodexCloudConfig,
    context: &CloudSetupContext<'_>,
) -> HashMap<String, String> {
    let mut env_vars = HashMap::new();
    let container_tier = context
        .env_vars
        .get(AGENT_ISOLATION_TIER_ENV)
        .is_some_and(|tier| tier.trim() == "container");
    if !container_tier {
        for key in SETUP_ENV_ALLOWLIST {
            if let Ok(value) = harness_core::config::process_env::var(key) {
                env_vars.insert(key.to_string(), value);
            }
        }
    }
    for key in [
        AGENT_ISOLATION_TIER_ENV,
        AGENT_NETWORK_ALLOWLIST_ENV,
        AGENT_CONTAINER_IMAGE_ENV,
        AGENT_EGRESS_PROXY_IMAGE_ENV,
    ] {
        if let Some(value) = context.env_vars.get(key) {
            env_vars.insert(key.to_string(), value.clone());
        }
    }
    for key in &cloud.setup_secret_env {
        if let Ok(value) = harness_core::config::process_env::var(key) {
            env_vars.insert(key.clone(), value);
        }
    }
    env_vars
}

async fn run_setup_command(
    cloud: &CodexCloudConfig,
    context: &CloudSetupContext<'_>,
    setup_command: &str,
    secret_state: Option<&container_state::SecretSetupState>,
) -> harness_core::error::Result<crate::BoundedOutput> {
    crate::spawn_supervisor::validate_capability_token(context.capability_token)?;
    let setup_sandbox_mode = setup_sandbox_mode(context.sandbox_mode);
    let sandbox_spec = if let Some(token) = context.capability_token {
        SandboxSpec::new(setup_sandbox_mode, context.project_root)
            .with_allowed_write_paths(token.allowed_write_paths.clone())
    } else {
        SandboxSpec::new(setup_sandbox_mode, context.project_root)
    };
    let mut env_vars = setup_spawn_env(cloud, context);
    let container_bind_mounts = if let Some(state) = secret_state {
        state.apply(&mut env_vars)
    } else {
        apply_container_state(cloud, context.project_root, &mut env_vars)?
    };
    let args = [OsString::from("-lc"), OsString::from(setup_command)];
    let mut spawn =
        crate::spawn_contract::prepare_agent_spawn(crate::spawn_contract::AgentSpawnInput {
            program: Path::new("sh"),
            args: &args,
            project_root: context.project_root,
            sandbox_spec: &sandbox_spec,
            env_vars: &env_vars,
            secret_env_keys: &cloud.setup_secret_env,
            container_bind_mounts: &container_bind_mounts,
            permission_mode: context.permission_mode,
            forward_stdin: false,
        })
        .await?;
    spawn.clear_inherited_env = true;

    let mut cmd = Command::new(&spawn.program);
    cmd.args(&spawn.args)
        .current_dir(&spawn.current_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    crate::set_process_group(&mut cmd);
    crate::spawn_contract::apply_process_env(&mut cmd, &spawn);
    let child = cmd.spawn().map_err(|error| {
        HarnessError::AgentExecution(format!(
            "failed to run cloud setup command `{setup_command}`: {error}"
        ))
    })?;
    let mut child = crate::ManagedChild::new(child, "codex cloud setup")
        .with_egress_proxy_lease(spawn.egress_proxy_lease.clone())
        .with_egress_verification(spawn.egress_verification);
    let secret_values: Vec<String> = cloud
        .setup_secret_env
        .iter()
        .filter_map(|key| harness_core::config::process_env::var(key).ok())
        .filter(|value| !value.is_empty())
        .collect();
    child
        .wait_with_redacted_output(
            &crate::OutputLimits {
                idle_timeout: None,
                max_captured_bytes: SETUP_CAPTURE_MAX_BYTES,
            },
            &secret_values,
        )
        .await
        .map_err(|error| {
            HarnessError::AgentExecution(format!(
                "failed to wait for cloud setup command `{setup_command}`: {error}"
            ))
        })
}

pub(crate) async fn run_setup_phase(
    cloud: &CodexCloudConfig,
    context: CloudSetupContext<'_>,
) -> harness_core::error::Result<()> {
    if !cloud.enabled || cloud.setup_commands.is_empty() {
        return Ok(());
    }

    if setup_cache_is_fresh(cloud, context.project_root)? {
        return Ok(());
    }

    let secret_state = container_state::SecretSetupState::create(cloud, &context)?;
    let discards_container_state = secret_state.is_some();
    let setup_result = async {
        for setup_command in &cloud.setup_commands {
            if setup_command.trim().is_empty() {
                continue;
            }
            validate_setup_command(setup_command).map_err(HarnessError::AgentExecution)?;
            let output =
                run_setup_command(cloud, &context, setup_command, secret_state.as_ref()).await?;
            if !output.status.success() {
                let detail = command_output_summary_bytes(
                    &output.stdout,
                    &output.stderr,
                    &cloud.setup_secret_env,
                );
                return Err(HarnessError::AgentExecution(format!(
                    "cloud setup command `{setup_command}` failed with {}: {detail}",
                    output.status
                )));
            }
        }
        Ok(())
    }
    .await;
    if let Some(state) = secret_state {
        state.finish(setup_result)?;
    } else {
        setup_result?;
    }

    if !discards_container_state {
        write_setup_cache_stamp(cloud, context.project_root)?;
    }
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

#[cfg(test)]
fn command_output_summary(output: &std::process::Output, secret_env: &[String]) -> String {
    command_output_summary_bytes(&output.stdout, &output.stderr, secret_env)
}

fn command_output_summary_bytes(stdout: &[u8], stderr: &[u8], secret_env: &[String]) -> String {
    let secret_values: Vec<String> = secret_env
        .iter()
        .filter_map(|key| std::env::var(key).ok())
        .filter(|value| !value.is_empty())
        .collect();
    command_output_summary_with_secret_values(stdout, stderr, &secret_values)
}

fn command_output_summary_with_secret_values(
    stdout: &[u8],
    stderr: &[u8],
    secret_values: &[String],
) -> String {
    let stderr = String::from_utf8_lossy(stderr).trim().to_string();
    let stdout = String::from_utf8_lossy(stdout).trim().to_string();
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
    fn trusted_setup_can_write_for_read_only_agent_modes() {
        assert_eq!(
            setup_sandbox_mode(SandboxMode::ReadOnly),
            SandboxMode::WorkspaceWrite
        );
        assert_eq!(
            setup_sandbox_mode(SandboxMode::ReadOnlyWithNetwork),
            SandboxMode::WorkspaceWrite
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

        let summary = command_output_summary_with_secret_values(
            &output.stdout,
            &output.stderr,
            &[secret_value.to_string()],
        );

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

    #[test]
    fn setup_environment_preserves_only_spawn_controls_for_container_requests() {
        let cloud = CodexCloudConfig::default();
        let request_env = HashMap::from([
            (
                AGENT_ISOLATION_TIER_ENV.to_string(),
                "container".to_string(),
            ),
            (
                AGENT_NETWORK_ALLOWLIST_ENV.to_string(),
                "api.openai.com".to_string(),
            ),
            (
                "REQUEST_ONLY_VALUE".to_string(),
                "not-for-setup".to_string(),
            ),
        ]);
        let context = CloudSetupContext {
            project_root: Path::new("/tmp/project"),
            sandbox_mode: SandboxMode::ReadOnly,
            permission_mode: AgentPermissionMode::Scoped,
            env_vars: &request_env,
            capability_token: None,
        };

        let setup_env = setup_spawn_env(&cloud, &context);

        assert_eq!(setup_env[AGENT_ISOLATION_TIER_ENV], "container");
        assert_eq!(setup_env[AGENT_NETWORK_ALLOWLIST_ENV], "api.openai.com");
        assert!(!setup_env.contains_key("REQUEST_ONLY_VALUE"));
        assert!(!setup_env.contains_key("PATH"));
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

        run_test_setup(&cloud, dir.path()).await?;

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

        run_test_setup(&cloud, dir.path()).await?;
        Ok(())
    }

    #[tokio::test]
    async fn read_only_agent_context_allows_setup_workspace_write() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let marker = dir.path().join("setup-complete");
        let cloud = CodexCloudConfig {
            enabled: true,
            cache_ttl_hours: 0,
            setup_commands: vec![format!("touch '{}'", marker.display())],
            setup_secret_env: Vec::new(),
        };
        let env_vars = HashMap::new();

        run_setup_phase(
            &cloud,
            CloudSetupContext {
                project_root: dir.path(),
                sandbox_mode: SandboxMode::ReadOnly,
                permission_mode: AgentPermissionMode::Full,
                env_vars: &env_vars,
                capability_token: None,
            },
        )
        .await?;

        assert!(marker.is_file());
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

        let result = run_test_setup(&cloud, dir.path()).await;
        assert!(result.is_err(), "chaining command must be rejected");
        Ok(())
    }

    #[tokio::test]
    #[ignore = "requires the reference agent Docker image"]
    async fn secret_backed_container_setup_does_not_cache_discarded_state() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let count = dir.path().join("setup-count");
        let cloud = CodexCloudConfig {
            enabled: true,
            cache_ttl_hours: 12,
            setup_commands: vec!["printf x >> setup-count".to_string()],
            setup_secret_env: vec!["HARNESS_TEST_SETUP_TOKEN".to_string()],
        };
        let env_vars = HashMap::from([
            (
                AGENT_ISOLATION_TIER_ENV.to_string(),
                "container".to_string(),
            ),
            (
                AGENT_CONTAINER_IMAGE_ENV.to_string(),
                "harness-agent:gh1771".to_string(),
            ),
        ]);
        let run = || {
            run_setup_phase(
                &cloud,
                CloudSetupContext {
                    project_root: dir.path(),
                    sandbox_mode: SandboxMode::DangerFullAccess,
                    permission_mode: AgentPermissionMode::Full,
                    env_vars: &env_vars,
                    capability_token: None,
                },
            )
        };

        run().await?;
        run().await?;

        assert_eq!(fs::read_to_string(count)?, "xx");
        Ok(())
    }

    #[tokio::test]
    #[ignore = "requires the reference agent Docker image"]
    async fn secret_backed_container_setup_masks_persistent_state() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let persistent_state = dir.path().join(".harness/cloud-setup-state");
        fs::create_dir_all(&persistent_state)?;
        fs::write(persistent_state.join("existing"), b"secret")?;
        let cloud = CodexCloudConfig {
            enabled: true,
            cache_ttl_hours: 12,
            setup_commands: vec!["test ! -e .harness/cloud-setup-state/existing".to_string()],
            setup_secret_env: vec!["HARNESS_TEST_SETUP_TOKEN".to_string()],
        };
        let env_vars = HashMap::from([
            (
                AGENT_ISOLATION_TIER_ENV.to_string(),
                "container".to_string(),
            ),
            (
                AGENT_CONTAINER_IMAGE_ENV.to_string(),
                "harness-agent:gh1771".to_string(),
            ),
        ]);

        run_setup_phase(
            &cloud,
            CloudSetupContext {
                project_root: dir.path(),
                sandbox_mode: SandboxMode::DangerFullAccess,
                permission_mode: AgentPermissionMode::Full,
                env_vars: &env_vars,
                capability_token: None,
            },
        )
        .await?;

        assert_eq!(fs::read(persistent_state.join("existing"))?, b"secret");
        Ok(())
    }

    async fn run_test_setup(cloud: &CodexCloudConfig, project_root: &Path) -> anyhow::Result<()> {
        let env_vars = HashMap::new();
        run_setup_phase(
            cloud,
            CloudSetupContext {
                project_root,
                sandbox_mode: SandboxMode::DangerFullAccess,
                permission_mode: AgentPermissionMode::Full,
                env_vars: &env_vars,
                capability_token: None,
            },
        )
        .await?;
        Ok(())
    }
}
