use super::{setup_cache_key, CloudSetupContext};
use crate::spawn_contract::ContainerBindMount;
use harness_core::agent::AGENT_ISOLATION_TIER_ENV;
use harness_core::config::agents::CodexCloudConfig;
use harness_core::error::HarnessError;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const CONTAINER_CLOUD_HOME: &str = "/harness-cloud-home";
const CONTAINER_CLOUD_TMP: &str = "/tmp";
const CONTAINER_STATE_MASK: &str = "/workspace/.harness/cloud-setup-state";

fn is_container_tier(env_vars: &HashMap<String, String>) -> bool {
    env_vars
        .get(AGENT_ISOLATION_TIER_ENV)
        .is_some_and(|tier| tier.trim() == "container")
}

fn container_state_root(cloud: &CodexCloudConfig, project_root: &Path) -> PathBuf {
    project_root
        .join(".harness/cloud-setup-state")
        .join(setup_cache_key(cloud, project_root))
}

fn create_workspace_state_dir(
    project_root: &Path,
    path: &Path,
    unix_mode: u32,
) -> harness_core::error::Result<()> {
    let relative = path.strip_prefix(project_root).map_err(|_| {
        HarnessError::AgentExecution(format!(
            "container state path `{}` is outside project root `{}`",
            path.display(),
            project_root.display()
        ))
    })?;
    let mut current = project_root.to_path_buf();
    for component in relative.components() {
        current.push(component);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(HarnessError::AgentExecution(format!(
                    "container state path `{}` must not contain symbolic links",
                    current.display()
                )));
            }
            Ok(metadata) if !metadata.is_dir() => {
                return Err(HarnessError::AgentExecution(format!(
                    "container state path `{}` is not a directory",
                    current.display()
                )));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                fs::create_dir(&current).map_err(|error| state_error("create", &current, error))?;
            }
            Err(error) => return Err(state_error("inspect", &current, error)),
        }
    }
    set_container_writable(path, unix_mode)
}

fn set_container_writable(path: &Path, unix_mode: u32) -> harness_core::error::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(unix_mode))
            .map_err(|error| state_error("make container-writable", path, error))?;
    }
    #[cfg(not(unix))]
    let _ = unix_mode;
    Ok(())
}

fn state_error(action: &str, path: &Path, error: std::io::Error) -> HarnessError {
    HarnessError::AgentExecution(format!(
        "failed to {action} persistent cloud setup state `{}`: {error}",
        path.display()
    ))
}

fn set_container_state_env(env_vars: &mut HashMap<String, String>) {
    for (key, value) in [
        ("HOME", CONTAINER_CLOUD_HOME),
        ("TMPDIR", CONTAINER_CLOUD_TMP),
        ("TMP", CONTAINER_CLOUD_TMP),
        ("TEMP", CONTAINER_CLOUD_TMP),
    ] {
        env_vars.insert(key.to_string(), value.to_string());
    }
}

pub(crate) fn apply_container_state(
    cloud: &CodexCloudConfig,
    project_root: &Path,
    env_vars: &mut HashMap<String, String>,
) -> harness_core::error::Result<Vec<ContainerBindMount>> {
    if !cloud.enabled || !is_container_tier(env_vars) {
        return Ok(Vec::new());
    }

    let mask = project_root
        .join(".harness/cloud-setup-mask")
        .join(setup_cache_key(cloud, project_root));
    create_workspace_state_dir(project_root, &mask, 0o755)?;
    let mut mounts = vec![ContainerBindMount::workspace_read_only(
        mask,
        PathBuf::from(CONTAINER_STATE_MASK),
    )];
    if !cloud
        .setup_commands
        .iter()
        .any(|command| !command.trim().is_empty())
    {
        return Ok(mounts);
    }

    let state_root = container_state_root(cloud, project_root);
    let home = state_root.join("home");
    let temporary = state_root.join("tmp");
    create_workspace_state_dir(project_root, &home, 0o777)?;
    create_workspace_state_dir(project_root, &temporary, 0o1777)?;
    set_container_state_env(env_vars);
    mounts.extend([
        ContainerBindMount::workspace(home, PathBuf::from(CONTAINER_CLOUD_HOME)),
        ContainerBindMount::workspace(temporary, PathBuf::from(CONTAINER_CLOUD_TMP)),
    ]);
    Ok(mounts)
}

pub(super) struct SecretSetupState {
    directory: tempfile::TempDir,
    cleanup_image: Option<String>,
}

impl SecretSetupState {
    pub(super) fn create(
        cloud: &CodexCloudConfig,
        context: &CloudSetupContext<'_>,
    ) -> harness_core::error::Result<Option<Self>> {
        if cloud.setup_secret_env.is_empty() {
            return Ok(None);
        }
        let container_tier = is_container_tier(context.env_vars);
        let directory = tempfile::Builder::new()
            .prefix("harness-cloud-setup-")
            .tempdir()
            .map_err(|error| {
                state_error("create secret", Path::new("temporary directory"), error)
            })?;
        let modes = if container_tier {
            [("home", 0o777), ("tmp", 0o1777)]
        } else {
            [("home", 0o700), ("tmp", 0o700)]
        };
        for (name, mode) in modes {
            let path = directory.path().join(name);
            fs::create_dir(&path).map_err(|error| state_error("create secret", &path, error))?;
            set_container_writable(&path, mode)?;
        }
        Ok(Some(Self {
            directory,
            cleanup_image: container_tier
                .then(|| crate::spawn_contract::agent_container_image(context.env_vars)),
        }))
    }

    pub(super) fn apply(&self, env_vars: &mut HashMap<String, String>) -> Vec<ContainerBindMount> {
        if self.cleanup_image.is_some() {
            set_container_state_env(env_vars);
            vec![
                ContainerBindMount::harness_temp(
                    self.directory.path().join("home"),
                    PathBuf::from(CONTAINER_CLOUD_HOME),
                ),
                ContainerBindMount::harness_temp(
                    self.directory.path().join("tmp"),
                    PathBuf::from(CONTAINER_CLOUD_TMP),
                ),
            ]
        } else {
            let home = self.directory.path().join("home");
            let temporary = self.directory.path().join("tmp");
            for (key, value) in [
                ("HOME", &home),
                ("TMPDIR", &temporary),
                ("TMP", &temporary),
                ("TEMP", &temporary),
            ] {
                env_vars.insert(key.to_string(), value.to_string_lossy().into_owned());
            }
            Vec::new()
        }
    }

    pub(super) fn finish(
        self,
        setup_result: harness_core::error::Result<()>,
    ) -> harness_core::error::Result<()> {
        let root = self.directory.path().to_path_buf();
        let cleanup_result = match (self.directory.close(), self.cleanup_image.as_deref()) {
            (Ok(()), _) => Ok(()),
            (Err(host_error), Some(image)) => cleanup_container_owned_state(image, &root)
                .and_then(|()| {
                    fs::remove_dir_all(&root)
                        .map_err(|error| state_error("remove secret", &root, error))
                })
                .map_err(|fallback_error| {
                    HarnessError::AgentExecution(format!(
                        "failed to remove secret cloud setup state: {host_error}; container cleanup also failed: {fallback_error}"
                    ))
                }),
            (Err(error), None) => Err(state_error("remove secret", &root, error)),
        };
        match (setup_result, cleanup_result) {
            (Err(setup), Err(cleanup)) => Err(HarnessError::AgentExecution(format!(
                "{setup}; additionally, {cleanup}"
            ))),
            (Err(error), _) | (_, Err(error)) => Err(error),
            (Ok(()), Ok(())) => Ok(()),
        }
    }
}

fn cleanup_container_owned_state(image: &str, root: &Path) -> harness_core::error::Result<()> {
    let home = root.join("home");
    let temporary = root.join("tmp");
    ensure_cleanup_mount(&home, 0o777)?;
    ensure_cleanup_mount(&temporary, 0o1777)?;
    let home_mount = format!("type=bind,src={},dst=/harness-secret-home", home.display());
    let temporary_mount = format!(
        "type=bind,src={},dst=/harness-secret-tmp",
        temporary.display()
    );
    const CLEANUP_SCRIPT: &str = r#"for root in /harness-secret-home /harness-secret-tmp; do
  find "$root" -mindepth 1 -type d -exec chmod u+rwx '{}' +
  find "$root" -mindepth 1 -depth -delete
done"#;
    let status = Command::new("docker")
        .args([
            "run",
            "--rm",
            "--network",
            "none",
            "--read-only",
            "--cap-drop",
            "ALL",
            "--security-opt",
            "no-new-privileges",
            "--mount",
            &home_mount,
            "--mount",
            &temporary_mount,
            "--entrypoint",
            "sh",
            image,
            "-c",
            CLEANUP_SCRIPT,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|error| {
            HarnessError::AgentExecution(format!(
                "failed to start secret state cleanup container: {error}"
            ))
        })?;
    if status.success() {
        Ok(())
    } else {
        Err(HarnessError::AgentExecution(format!(
            "secret state cleanup container exited with {status}"
        )))
    }
}

fn ensure_cleanup_mount(path: &Path, unix_mode: u32) -> harness_core::error::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => Ok(()),
        Ok(_) => Err(HarnessError::AgentExecution(format!(
            "secret state cleanup path `{}` is not a directory",
            path.display()
        ))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir(path).map_err(|error| state_error("recreate secret", path, error))?;
            set_container_writable(path, unix_mode)
        }
        Err(error) => Err(state_error("inspect secret", path, error)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_core::config::agents::{AgentPermissionMode, SandboxMode};

    fn cloud(commands: Vec<String>) -> CodexCloudConfig {
        CodexCloudConfig {
            enabled: true,
            cache_ttl_hours: 12,
            setup_commands: commands,
            setup_secret_env: Vec::new(),
        }
    }

    #[test]
    fn persistent_state_is_writable_and_hidden_from_workspace() -> anyhow::Result<()> {
        let project = tempfile::tempdir()?;
        let mut env = HashMap::from([(
            AGENT_ISOLATION_TIER_ENV.to_string(),
            "container".to_string(),
        )]);
        let mounts = apply_container_state(
            &cloud(vec!["cargo fetch".to_string()]),
            project.path(),
            &mut env,
        )?;
        assert_eq!(mounts.len(), 3);
        assert_eq!(mounts[0].destination, Path::new(CONTAINER_STATE_MASK));
        assert_eq!(env["HOME"], CONTAINER_CLOUD_HOME);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&mounts[1].source)?.permissions().mode() & 0o777,
                0o777
            );
            assert_eq!(
                fs::metadata(&mounts[2].source)?.permissions().mode() & 0o1777,
                0o1777
            );
        }
        Ok(())
    }

    #[test]
    fn state_mask_is_applied_without_setup_commands() -> anyhow::Result<()> {
        let project = tempfile::tempdir()?;
        let mut env = HashMap::from([(
            AGENT_ISOLATION_TIER_ENV.to_string(),
            "container".to_string(),
        )]);
        let mounts = apply_container_state(&cloud(Vec::new()), project.path(), &mut env)?;
        assert_eq!(mounts.len(), 1);
        assert!(!env.contains_key("HOME"));
        assert!(!project.path().join(".harness/cloud-setup-state").exists());
        Ok(())
    }

    #[test]
    fn secret_state_is_external_and_removed_after_setup() -> anyhow::Result<()> {
        let project = tempfile::tempdir()?;
        let cloud = CodexCloudConfig {
            setup_secret_env: vec!["NPM_TOKEN".to_string()],
            ..cloud(vec!["npm ci".to_string()])
        };
        let env = HashMap::from([(
            AGENT_ISOLATION_TIER_ENV.to_string(),
            "container".to_string(),
        )]);
        let context = CloudSetupContext {
            project_root: project.path(),
            sandbox_mode: SandboxMode::ReadOnly,
            permission_mode: AgentPermissionMode::Scoped,
            env_vars: &env,
            capability_token: None,
        };
        let Some(state) = SecretSetupState::create(&cloud, &context)? else {
            anyhow::bail!("container setup with configured secrets must create isolated state");
        };
        let mut setup_env = HashMap::new();
        let mounts = state.apply(&mut setup_env);
        let Some(root) = mounts[0].source.parent().map(Path::to_path_buf) else {
            anyhow::bail!("secret state mount must have a parent");
        };

        assert!(!root.starts_with(project.path()));
        assert_eq!(setup_env["HOME"], CONTAINER_CLOUD_HOME);
        state.finish(Ok(()))?;
        assert!(!root.exists());
        Ok(())
    }

    #[test]
    fn secret_state_isolates_host_home_and_tmp() -> anyhow::Result<()> {
        let project = tempfile::tempdir()?;
        let cloud = CodexCloudConfig {
            setup_secret_env: vec!["NPM_TOKEN".to_string()],
            ..cloud(vec!["npm ci".to_string()])
        };
        let env = HashMap::new();
        let context = CloudSetupContext {
            project_root: project.path(),
            sandbox_mode: SandboxMode::DangerFullAccess,
            permission_mode: AgentPermissionMode::Full,
            env_vars: &env,
            capability_token: None,
        };
        let Some(state) = SecretSetupState::create(&cloud, &context)? else {
            anyhow::bail!("host setup with configured secrets must create isolated state");
        };
        let root = state.directory.path().to_path_buf();
        let mut setup_env = HashMap::new();

        assert!(state.apply(&mut setup_env).is_empty());
        assert_eq!(Path::new(&setup_env["HOME"]), root.join("home"));
        assert_eq!(Path::new(&setup_env["TMPDIR"]), root.join("tmp"));

        fs::write(root.join("home/credential"), b"secret")?;
        state.finish(Ok(()))?;
        assert!(!root.exists());
        Ok(())
    }

    #[test]
    #[cfg(unix)]
    #[ignore = "requires the reference agent Docker image for fallback cleanup"]
    fn secret_state_cleanup_handles_container_owned_directories() -> anyhow::Result<()> {
        use std::os::unix::fs::PermissionsExt;

        let project = tempfile::tempdir()?;
        let cloud = CodexCloudConfig {
            setup_secret_env: vec!["NPM_TOKEN".to_string()],
            ..cloud(vec!["npm ci".to_string()])
        };
        let env = HashMap::from([
            (
                AGENT_ISOLATION_TIER_ENV.to_string(),
                "container".to_string(),
            ),
            (
                harness_core::agent::AGENT_CONTAINER_IMAGE_ENV.to_string(),
                "harness-agent:gh1771".to_string(),
            ),
        ]);
        let context = CloudSetupContext {
            project_root: project.path(),
            sandbox_mode: SandboxMode::ReadOnly,
            permission_mode: AgentPermissionMode::Scoped,
            env_vars: &env,
            capability_token: None,
        };
        let state = SecretSetupState::create(&cloud, &context)?
            .ok_or_else(|| anyhow::anyhow!("missing secret state"))?;
        let root = state.directory.path().to_path_buf();
        let nested = root.join("home/container-owned");
        fs::create_dir(&nested)?;
        fs::write(nested.join("credential"), b"secret")?;
        fs::set_permissions(&nested, fs::Permissions::from_mode(0o555))?;
        fs::remove_dir(root.join("tmp"))?;

        let result = state.finish(Ok(()));
        if root.exists() {
            fs::set_permissions(&nested, fs::Permissions::from_mode(0o755))?;
            fs::remove_dir_all(&root)?;
        }

        result?;
        assert!(!root.exists());
        Ok(())
    }
}
