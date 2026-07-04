use harness_core::agent::{AGENT_ISOLATION_TIER_ENV, AGENT_NETWORK_ALLOWLIST_ENV};
use harness_core::config::isolation::IsolationTier;
use harness_core::error::HarnessError;
use harness_core::run_id::{AGENT_RUN_ID_ENV, AGENT_RUN_PARENT_ENV};
use harness_sandbox::{wrap_command, SandboxEngine, SandboxSpec};
use std::collections::{BTreeMap, HashMap};
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

const DEFAULT_AGENT_CONTAINER_IMAGE: &str = "harness-agent:latest";
const AGENT_CONTAINER_IMAGE_ENV: &str = "HARNESS_AGENT_CONTAINER_IMAGE";
const AGENT_EGRESS_PROXY_ENV: &str = "HARNESS_AGENT_EGRESS_PROXY";
const CONTAINER_EGRESS_ALLOWLIST_ENV: &str = "HARNESS_AGENT_EGRESS_ALLOWLIST";
const CONTAINER_WORKSPACE: &str = "/workspace";

pub(crate) struct AgentSpawnInput<'a> {
    pub(crate) program: &'a Path,
    pub(crate) args: &'a [OsString],
    pub(crate) project_root: &'a Path,
    pub(crate) sandbox_spec: &'a SandboxSpec,
    pub(crate) env_vars: &'a HashMap<String, String>,
}

#[derive(Debug, Clone)]
pub(crate) struct PreparedAgentSpawn {
    pub(crate) program: PathBuf,
    pub(crate) args: Vec<OsString>,
    pub(crate) current_dir: PathBuf,
    pub(crate) process_env: BTreeMap<String, String>,
    pub(crate) clear_inherited_env: bool,
    pub(crate) sandbox_engine: SandboxEngine,
}

pub(crate) trait AgentSpawnContract {
    fn prepare(&self, input: AgentSpawnInput<'_>) -> Result<PreparedAgentSpawn, HarnessError>;
}

pub(crate) struct HostSpawn;

impl AgentSpawnContract for HostSpawn {
    fn prepare(&self, input: AgentSpawnInput<'_>) -> Result<PreparedAgentSpawn, HarnessError> {
        let wrapped_command =
            wrap_command(input.program, input.args, input.sandbox_spec).map_err(|error| {
                HarnessError::AgentExecution(format!("sandbox setup failed for agent: {error}"))
            })?;
        Ok(PreparedAgentSpawn {
            program: wrapped_command.program,
            args: wrapped_command.args,
            current_dir: input.project_root.to_path_buf(),
            process_env: host_process_env(input.env_vars),
            clear_inherited_env: false,
            sandbox_engine: wrapped_command.engine,
        })
    }
}

pub(crate) struct ContainerSpawn;

impl AgentSpawnContract for ContainerSpawn {
    fn prepare(&self, input: AgentSpawnInput<'_>) -> Result<PreparedAgentSpawn, HarnessError> {
        let workspace = canonical_workspace(input.project_root)?;
        let tier = isolation_tier(input.env_vars)?;
        if tier != IsolationTier::Container {
            return Err(HarnessError::AgentExecution(format!(
                "container spawn received non-container isolation tier `{}`",
                tier.as_str()
            )));
        }

        let allowlist = network_allowlist(input.env_vars);
        let image = container_image(input.env_vars);
        let mut args = vec![OsString::from("run"), OsString::from("--rm")];
        args.push(OsString::from("--workdir"));
        args.push(OsString::from(CONTAINER_WORKSPACE));
        args.push(OsString::from("--mount"));
        args.push(OsString::from(format!(
            "type=bind,src={},dst={CONTAINER_WORKSPACE}",
            workspace.display()
        )));
        args.push(OsString::from("--network"));
        args.push(OsString::from(container_network_mode(
            input.env_vars,
            &allowlist,
        )));
        for (key, value) in container_env_vars(input.env_vars) {
            args.push(OsString::from("--env"));
            args.push(OsString::from(format!("{key}={value}")));
        }
        if !allowlist.is_empty() {
            args.push(OsString::from("--env"));
            args.push(OsString::from(format!(
                "{CONTAINER_EGRESS_ALLOWLIST_ENV}={}",
                allowlist.join(",")
            )));
        }
        if let Some(proxy) = egress_proxy(input.env_vars) {
            for key in ["HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY"] {
                args.push(OsString::from("--env"));
                args.push(OsString::from(format!("{key}={proxy}")));
            }
            args.push(OsString::from("--env"));
            args.push(OsString::from("NO_PROXY=localhost,127.0.0.1"));
        }
        args.push(OsString::from(image));
        args.push(container_program(input.program));
        args.extend(input.args.iter().cloned());

        Ok(PreparedAgentSpawn {
            program: PathBuf::from("docker"),
            args,
            current_dir: workspace,
            process_env: docker_process_env(),
            clear_inherited_env: true,
            sandbox_engine: SandboxEngine::None,
        })
    }
}

pub(crate) fn prepare_agent_spawn(
    input: AgentSpawnInput<'_>,
) -> Result<PreparedAgentSpawn, HarnessError> {
    match isolation_tier(input.env_vars)? {
        IsolationTier::Host => HostSpawn.prepare(input),
        IsolationTier::Container => ContainerSpawn.prepare(input),
        IsolationTier::Microvm => Err(HarnessError::AgentExecution(
            "isolation tier `microvm` is reserved but not implemented".to_string(),
        )),
    }
}

pub(crate) fn apply_process_env(cmd: &mut tokio::process::Command, spawn: &PreparedAgentSpawn) {
    if spawn.clear_inherited_env {
        cmd.env_clear();
    }
    cmd.envs(spawn.process_env.iter());
}

fn isolation_tier(env_vars: &HashMap<String, String>) -> Result<IsolationTier, HarnessError> {
    match env_vars.get(AGENT_ISOLATION_TIER_ENV).map(String::as_str) {
        None | Some("") | Some("host") => Ok(IsolationTier::Host),
        Some("container") => Ok(IsolationTier::Container),
        Some("microvm") => Ok(IsolationTier::Microvm),
        Some(other) => Err(HarnessError::AgentExecution(format!(
            "unknown isolation tier `{other}`"
        ))),
    }
}

fn network_allowlist(env_vars: &HashMap<String, String>) -> Vec<String> {
    env_vars
        .get(AGENT_NETWORK_ALLOWLIST_ENV)
        .map(String::as_str)
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

fn host_process_env(env_vars: &HashMap<String, String>) -> BTreeMap<String, String> {
    env_vars
        .iter()
        .filter(|(key, _)| !is_spawn_control_env(key))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect()
}

fn container_env_vars(env_vars: &HashMap<String, String>) -> BTreeMap<String, String> {
    env_vars
        .iter()
        .filter(|(key, _)| !is_spawn_control_env(key))
        .filter(|(key, _)| !is_operator_secret_env(key))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect()
}

fn docker_process_env() -> BTreeMap<String, String> {
    std::env::var("PATH")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .map(|path| BTreeMap::from([("PATH".to_string(), path)]))
        .unwrap_or_default()
}

fn is_spawn_control_env(key: &str) -> bool {
    matches!(
        key,
        AGENT_ISOLATION_TIER_ENV
            | AGENT_NETWORK_ALLOWLIST_ENV
            | AGENT_CONTAINER_IMAGE_ENV
            | AGENT_EGRESS_PROXY_ENV
    )
}

fn is_operator_secret_env(key: &str) -> bool {
    if key == AGENT_RUN_ID_ENV || key == AGENT_RUN_PARENT_ENV || key.starts_with("HARNESS_SCOPED_")
    {
        return false;
    }
    let key = key.to_ascii_uppercase();
    key == "GITHUB_TOKEN"
        || key == "GH_TOKEN"
        || key.ends_with("_TOKEN")
        || key.contains("API_KEY")
        || key.contains("SECRET")
        || key.contains("PASSWORD")
}

fn container_image(env_vars: &HashMap<String, String>) -> String {
    env_vars
        .get(AGENT_CONTAINER_IMAGE_ENV)
        .map(String::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(DEFAULT_AGENT_CONTAINER_IMAGE)
        .to_string()
}

fn egress_proxy(env_vars: &HashMap<String, String>) -> Option<String> {
    env_vars
        .get(AGENT_EGRESS_PROXY_ENV)
        .map(String::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn container_network_mode(
    env_vars: &HashMap<String, String>,
    allowlist: &[String],
) -> &'static str {
    if allowlist.is_empty() || egress_proxy(env_vars).is_none() {
        "none"
    } else {
        "bridge"
    }
}

fn canonical_workspace(project_root: &Path) -> Result<PathBuf, HarnessError> {
    std::fs::canonicalize(project_root).map_err(|error| {
        HarnessError::AgentExecution(format!(
            "failed to resolve container workspace {}: {error}",
            project_root.display()
        ))
    })
}

fn container_program(program: &Path) -> OsString {
    if program.is_absolute() {
        program
            .file_name()
            .map(OsStr::to_os_string)
            .unwrap_or_else(|| program.as_os_str().to_os_string())
    } else {
        program.as_os_str().to_os_string()
    }
}

#[cfg(test)]
mod container_spawn_tests {
    use super::*;
    use harness_core::config::agents::SandboxMode;

    fn input<'a>(
        program: &'a Path,
        args: &'a [OsString],
        project_root: &'a Path,
        sandbox_spec: &'a SandboxSpec,
        env_vars: &'a HashMap<String, String>,
    ) -> AgentSpawnInput<'a> {
        AgentSpawnInput {
            program,
            args,
            project_root,
            sandbox_spec,
            env_vars,
        }
    }

    fn string_args(spawn: &PreparedAgentSpawn) -> Vec<String> {
        spawn
            .args
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn container_spawn_mounts_only_task_workspace() -> anyhow::Result<()> {
        let root = tempfile::tempdir()?;
        let mut env_vars = HashMap::new();
        env_vars.insert(
            AGENT_ISOLATION_TIER_ENV.to_string(),
            "container".to_string(),
        );
        env_vars.insert(
            AGENT_CONTAINER_IMAGE_ENV.to_string(),
            "example/agent:sha256-test".to_string(),
        );
        let args = vec![OsString::from("exec"), OsString::from("prompt")];
        let sandbox_spec = SandboxSpec::new(SandboxMode::WorkspaceWrite, root.path());

        let spawn = ContainerSpawn.prepare(input(
            Path::new("/opt/harness/bin/codex"),
            &args,
            root.path(),
            &sandbox_spec,
            &env_vars,
        ))?;

        let args = string_args(&spawn);
        assert_eq!(spawn.program, PathBuf::from("docker"));
        assert!(spawn.clear_inherited_env);
        assert_eq!(spawn.current_dir, std::fs::canonicalize(root.path())?);
        assert!(args.contains(&"--mount".to_string()));
        assert!(args.contains(&format!(
            "type=bind,src={},dst={CONTAINER_WORKSPACE}",
            std::fs::canonicalize(root.path())?.display()
        )));
        assert!(!args
            .iter()
            .any(|arg| arg.contains("/Users/") && arg.contains("home")));
        assert!(args.contains(&"--network".to_string()));
        assert!(args.contains(&"none".to_string()));
        assert!(args.contains(&"example/agent:sha256-test".to_string()));
        assert!(args.contains(&"codex".to_string()));
        Ok(())
    }

    #[test]
    fn container_spawn_applies_allowlist_proxy_contract() -> anyhow::Result<()> {
        let root = tempfile::tempdir()?;
        let mut env_vars = HashMap::new();
        env_vars.insert(
            AGENT_ISOLATION_TIER_ENV.to_string(),
            "container".to_string(),
        );
        env_vars.insert(
            AGENT_NETWORK_ALLOWLIST_ENV.to_string(),
            "github.com, api.anthropic.com".to_string(),
        );
        env_vars.insert(
            AGENT_EGRESS_PROXY_ENV.to_string(),
            "http://egress-proxy.local:8080".to_string(),
        );
        let sandbox_spec = SandboxSpec::new(SandboxMode::WorkspaceWrite, root.path());

        let spawn = ContainerSpawn.prepare(input(
            Path::new("claude"),
            &[],
            root.path(),
            &sandbox_spec,
            &env_vars,
        ))?;

        let args = string_args(&spawn);
        assert!(args.contains(&"bridge".to_string()));
        assert!(args.contains(&format!(
            "{CONTAINER_EGRESS_ALLOWLIST_ENV}=github.com,api.anthropic.com"
        )));
        assert!(args.contains(&"HTTPS_PROXY=http://egress-proxy.local:8080".to_string()));
        assert!(args.contains(&"ALL_PROXY=http://egress-proxy.local:8080".to_string()));
        Ok(())
    }

    #[test]
    fn container_spawn_keeps_network_closed_without_proxy_for_allowlist() -> anyhow::Result<()> {
        let root = tempfile::tempdir()?;
        let mut env_vars = HashMap::new();
        env_vars.insert(
            AGENT_ISOLATION_TIER_ENV.to_string(),
            "container".to_string(),
        );
        env_vars.insert(
            AGENT_NETWORK_ALLOWLIST_ENV.to_string(),
            "github.com".to_string(),
        );
        let sandbox_spec = SandboxSpec::new(SandboxMode::WorkspaceWrite, root.path());

        let spawn = ContainerSpawn.prepare(input(
            Path::new("codex"),
            &[],
            root.path(),
            &sandbox_spec,
            &env_vars,
        ))?;

        let args = string_args(&spawn);
        assert!(args.contains(&"none".to_string()));
        assert!(args.contains(&format!("{CONTAINER_EGRESS_ALLOWLIST_ENV}=github.com")));
        assert!(!args.iter().any(|arg| arg.starts_with("HTTPS_PROXY=")));
        Ok(())
    }

    #[test]
    fn container_spawn_filters_operator_env_secrets() -> anyhow::Result<()> {
        let root = tempfile::tempdir()?;
        let mut env_vars = HashMap::new();
        env_vars.insert(
            AGENT_ISOLATION_TIER_ENV.to_string(),
            "container".to_string(),
        );
        env_vars.insert(
            AGENT_RUN_ID_ENV.to_string(),
            "ar-01j00000000000000000000000".to_string(),
        );
        env_vars.insert("GITHUB_TOKEN".to_string(), "operator-token".to_string());
        env_vars.insert("ANTHROPIC_API_KEY".to_string(), "operator-key".to_string());
        env_vars.insert(
            "HARNESS_SCOPED_GITHUB_TOKEN".to_string(),
            "scoped-token".to_string(),
        );
        env_vars.insert(
            "CARGO_TARGET_DIR".to_string(),
            "/workspace/target".to_string(),
        );
        let sandbox_spec = SandboxSpec::new(SandboxMode::WorkspaceWrite, root.path());

        let spawn = ContainerSpawn.prepare(input(
            Path::new("codex"),
            &[],
            root.path(),
            &sandbox_spec,
            &env_vars,
        ))?;

        let args = string_args(&spawn);
        assert!(args.contains(&format!("{AGENT_RUN_ID_ENV}=ar-01j00000000000000000000000")));
        assert!(args.contains(&"HARNESS_SCOPED_GITHUB_TOKEN=scoped-token".to_string()));
        assert!(args.contains(&"CARGO_TARGET_DIR=/workspace/target".to_string()));
        assert!(!args.iter().any(|arg| arg.contains("operator-token")));
        assert!(!args.iter().any(|arg| arg.contains("operator-key")));
        assert!(!spawn.process_env.contains_key("GITHUB_TOKEN"));
        Ok(())
    }

    #[test]
    fn host_spawn_filters_spawn_control_env() -> anyhow::Result<()> {
        let root = tempfile::tempdir()?;
        let mut env_vars = HashMap::new();
        env_vars.insert(AGENT_ISOLATION_TIER_ENV.to_string(), "host".to_string());
        env_vars.insert("CARGO_TARGET_DIR".to_string(), "/tmp/target".to_string());
        let args = vec![OsString::from("exec")];
        let sandbox_spec = SandboxSpec::new(SandboxMode::DangerFullAccess, root.path());

        let spawn = prepare_agent_spawn(input(
            Path::new("codex"),
            &args,
            root.path(),
            &sandbox_spec,
            &env_vars,
        ))?;

        assert!(!spawn.clear_inherited_env);
        assert_eq!(
            spawn.process_env.get("CARGO_TARGET_DIR"),
            Some(&"/tmp/target".to_string())
        );
        assert!(!spawn.process_env.contains_key(AGENT_ISOLATION_TIER_ENV));
        assert_eq!(spawn.program, PathBuf::from("codex"));
        Ok(())
    }
}
