use super::{setup_cache_key, CloudSetupContext};
use crate::spawn_contract::ContainerBindMount;
use harness_core::agent::AGENT_ISOLATION_TIER_ENV;
use harness_core::config::agents::CodexCloudConfig;
use harness_core::error::HarnessError;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

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

pub(super) struct SecretContainerState {
    directory: tempfile::TempDir,
}

impl SecretContainerState {
    pub(super) fn create(
        cloud: &CodexCloudConfig,
        context: &CloudSetupContext<'_>,
    ) -> harness_core::error::Result<Option<Self>> {
        if cloud.setup_secret_env.is_empty() || !is_container_tier(context.env_vars) {
            return Ok(None);
        }
        let directory = tempfile::Builder::new()
            .prefix("harness-cloud-setup-")
            .tempdir()
            .map_err(|error| {
                state_error("create secret", Path::new("temporary directory"), error)
            })?;
        for (name, mode) in [("home", 0o777), ("tmp", 0o1777)] {
            let path = directory.path().join(name);
            fs::create_dir(&path).map_err(|error| state_error("create secret", &path, error))?;
            set_container_writable(&path, mode)?;
        }
        Ok(Some(Self { directory }))
    }

    pub(super) fn apply(&self, env_vars: &mut HashMap<String, String>) -> Vec<ContainerBindMount> {
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
    }

    pub(super) fn finish(
        self,
        setup_result: harness_core::error::Result<()>,
    ) -> harness_core::error::Result<()> {
        let cleanup_result = self.directory.close().map_err(|error| {
            HarnessError::AgentExecution(format!(
                "failed to remove secret cloud setup state: {error}"
            ))
        });
        match (setup_result, cleanup_result) {
            (Err(setup), Err(cleanup)) => Err(HarnessError::AgentExecution(format!(
                "{setup}; additionally, {cleanup}"
            ))),
            (Err(error), _) | (_, Err(error)) => Err(error),
            (Ok(()), Ok(())) => Ok(()),
        }
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
        let Some(state) = SecretContainerState::create(&cloud, &context)? else {
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
}
