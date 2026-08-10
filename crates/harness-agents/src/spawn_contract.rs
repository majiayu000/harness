use harness_core::agent::{AgentEgressMode, AgentRequest, AGENT_EGRESS_PROXY_IMAGE_ENV};
#[cfg(test)]
use harness_core::agent::{
    AGENT_CONTAINER_IMAGE_ENV, AGENT_ISOLATION_TIER_ENV, AGENT_NETWORK_ALLOWLIST_ENV,
};
use harness_core::config::agents::AgentPermissionMode;
use harness_core::config::agents::SandboxMode;
use harness_core::config::isolation::IsolationTier;
use harness_core::error::HarnessError;
use harness_core::run_id::{AGENT_RUN_ID_ENV, AGENT_RUN_PARENT_ENV};
use harness_sandbox::{wrap_command, NetworkPolicy, SandboxEngine, SandboxSpec};
use std::collections::{BTreeMap, HashMap};
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use sha2::{Digest, Sha256};

pub(crate) mod docker_ownership;
pub(crate) mod egress;
mod output_schema;
mod review_git;
mod spawn_env;
use docker_ownership::{append_os_labels, unique_resource_name, ManagedDockerResource};
use egress::{
    apply_proxy_env, container_canary_command, proxy_env_keys, EgressProxyLease, EgressProxyRoute,
    LEGACY_EGRESS_PROXY_ENV,
};
use spawn_env::{
    container_env_vars, container_image, docker_process_env, host_process_env, isolation_tier,
    network_allowlist, review_git_safe_workspace, ContainerEnv,
};

pub(crate) fn agent_container_image(env_vars: &HashMap<String, String>) -> String {
    container_image(env_vars)
}

/// Env keys Claude Code uses to detect that it is running nested inside
/// another Claude Code session; leaking any of them into a spawned agent
/// causes SIGTRAP. Only these markers are stripped — legitimate `CLAUDE_*`
/// configuration such as `CLAUDE_CONFIG_DIR` must pass through.
///
/// Keep in sync with the wrapper-variable classification in
/// `scripts/start-harness-codex-safe.sh`.
pub(crate) const NESTED_SESSION_ENV_KEYS: [&str; 5] = [
    "CLAUDECODE",
    "CLAUDE_CODE",
    "CLAUDE_CODE_ENTRYPOINT",
    "CLAUDE_CODE_SESSION_ID",
    "CLAUDE_SESSION_ID",
];

const DEFAULT_AGENT_CONTAINER_IMAGE: &str = "harness-agent:latest";
const CONTAINER_WORKSPACE: &str = "/workspace";
pub(crate) const REVIEW_GIT_SAFE_WORKSPACE_ENV: &str = "HARNESS_AGENT_REVIEW_GIT_SAFE_WORKSPACE";

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct AdapterSpawnPolicyFingerprint([u8; 32]);

pub(crate) fn adapter_spawn_policy_fingerprint(
    req: &AgentRequest,
    default_sandbox_mode: SandboxMode,
) -> AdapterSpawnPolicyFingerprint {
    let mut hasher = Sha256::new();
    hash_field(&mut hasher, b"adapter-spawn-policy/v1");
    hash_field(&mut hasher, req.project_root.as_os_str().as_encoded_bytes());
    hash_field(
        &mut hasher,
        &[match req.permission_mode {
            AgentPermissionMode::Scoped => 0,
            AgentPermissionMode::Full => 1,
        }],
    );
    hash_field(
        &mut hasher,
        &[match req.sandbox_mode.unwrap_or(default_sandbox_mode) {
            SandboxMode::ReadOnly => 0,
            SandboxMode::ReadOnlyWithNetwork => 1,
            SandboxMode::WorkspaceWrite => 2,
            SandboxMode::DangerFullAccess => 3,
        }],
    );

    let mut env_vars: Vec<_> = req
        .env_vars
        .iter()
        .filter(|(key, _)| key.as_str() != AGENT_RUN_ID_ENV && key.as_str() != AGENT_RUN_PARENT_ENV)
        .collect();
    env_vars.sort_unstable_by(|left, right| left.0.cmp(right.0));
    for (key, value) in env_vars {
        hash_field(&mut hasher, key.as_bytes());
        hash_field(&mut hasher, value.as_bytes());
    }

    if let Some(token) = &req.capability_token {
        hash_field(&mut hasher, b"capability");
        for path in &token.allowed_write_paths {
            hash_field(&mut hasher, path.as_os_str().as_encoded_bytes());
        }
    } else {
        hash_field(&mut hasher, b"no-capability");
    }

    AdapterSpawnPolicyFingerprint(hasher.finalize().into())
}

fn hash_field(hasher: &mut Sha256, value: &[u8]) {
    hasher.update((value.len() as u64).to_le_bytes());
    hasher.update(value);
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ContainerBindMount {
    pub(crate) source: PathBuf,
    pub(crate) destination: PathBuf,
    scope: ContainerBindMountScope,
    read_only: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ContainerBindMountScope {
    Workspace,
    HarnessTemp,
}

impl ContainerBindMount {
    pub(crate) fn workspace(source: PathBuf, destination: PathBuf) -> Self {
        Self {
            source,
            destination,
            scope: ContainerBindMountScope::Workspace,
            read_only: false,
        }
    }

    pub(crate) fn workspace_read_only(source: PathBuf, destination: PathBuf) -> Self {
        Self {
            source,
            destination,
            scope: ContainerBindMountScope::Workspace,
            read_only: true,
        }
    }

    pub(crate) fn harness_temp(source: PathBuf, destination: PathBuf) -> Self {
        Self {
            source,
            destination,
            scope: ContainerBindMountScope::HarnessTemp,
            read_only: false,
        }
    }

    pub(crate) fn harness_temp_read_only(source: PathBuf, destination: PathBuf) -> Self {
        Self {
            source,
            destination,
            scope: ContainerBindMountScope::HarnessTemp,
            read_only: true,
        }
    }
}

pub(crate) struct AgentSpawnInput<'a> {
    pub(crate) program: &'a Path,
    pub(crate) args: &'a [OsString],
    pub(crate) project_root: &'a Path,
    pub(crate) sandbox_spec: &'a SandboxSpec,
    pub(crate) env_vars: &'a HashMap<String, String>,
    pub(crate) secret_env_keys: &'a [String],
    pub(crate) container_bind_mounts: &'a [ContainerBindMount],
    pub(crate) permission_mode: AgentPermissionMode,
    /// The caller pipes the prompt through the child's stdin. The container
    /// tier must keep stdin open (`docker run -i`) or the prompt is silently
    /// dropped; the host tier inherits stdin either way.
    pub(crate) forward_stdin: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct PreparedAgentSpawn {
    pub(crate) program: PathBuf,
    pub(crate) args: Vec<OsString>,
    pub(crate) current_dir: PathBuf,
    /// Workspace path as seen by the child after host/container path mapping.
    pub(crate) child_workspace: PathBuf,
    pub(crate) process_env: BTreeMap<String, String>,
    pub(crate) clear_inherited_env: bool,
    pub(crate) sandbox_engine: SandboxEngine,
    pub(crate) egress_proxy_lease: Option<Arc<EgressProxyLease>>,
    pub(crate) egress_verification: EgressVerification,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EgressVerification {
    NotRequired,
    VerifiedBeforeSpawn,
    AwaitContainerCanary,
}

pub(crate) trait AgentSpawnContract {
    fn prepare(
        &self,
        input: AgentSpawnInput<'_>,
        egress_route: Option<&EgressProxyRoute>,
    ) -> Result<PreparedAgentSpawn, HarnessError>;
}

pub(crate) struct HostSpawn;

impl AgentSpawnContract for HostSpawn {
    fn prepare(
        &self,
        input: AgentSpawnInput<'_>,
        egress_route: Option<&EgressProxyRoute>,
    ) -> Result<PreparedAgentSpawn, HarnessError> {
        let allowlist = network_allowlist(input.env_vars);
        let egress_policy = AgentEgressMode::resolve(input.permission_mode, &allowlist);
        let network_policy = match egress_policy {
            AgentEgressMode::Unrestricted => NetworkPolicy::InheritSandboxMode,
            AgentEgressMode::DenyAll => NetworkPolicy::Deny,
            AgentEgressMode::FirstPartyProxy => NetworkPolicy::LocalProxy {
                port: egress_route
                    .and_then(EgressProxyRoute::local_proxy_port)
                    .ok_or_else(|| missing_proxy_route(IsolationTier::Host))?,
            },
        };
        let sandbox_spec = input
            .sandbox_spec
            .clone()
            .with_network_policy(network_policy);
        let wrapped_command =
            wrap_command(input.program, input.args, &sandbox_spec).map_err(|error| {
                HarnessError::AgentExecution(format!("sandbox setup failed for agent: {error}"))
            })?;
        let mut process_env = host_process_env(input.env_vars);
        if let Some(route) = egress_route {
            apply_proxy_env(&mut process_env, route.proxy_url());
        }
        Ok(PreparedAgentSpawn {
            program: wrapped_command.program,
            args: wrapped_command.args,
            current_dir: input.project_root.to_path_buf(),
            child_workspace: input.project_root.to_path_buf(),
            process_env,
            clear_inherited_env: false,
            sandbox_engine: wrapped_command.engine,
            egress_proxy_lease: None,
            egress_verification: if egress_route.is_some() {
                EgressVerification::VerifiedBeforeSpawn
            } else {
                EgressVerification::NotRequired
            },
        })
    }
}

pub(crate) struct ContainerSpawn;

impl AgentSpawnContract for ContainerSpawn {
    fn prepare(
        &self,
        input: AgentSpawnInput<'_>,
        egress_route: Option<&EgressProxyRoute>,
    ) -> Result<PreparedAgentSpawn, HarnessError> {
        let project_root = canonical_workspace(input.project_root)?;
        let tier = isolation_tier(input.env_vars)?;
        if tier != IsolationTier::Container {
            return Err(HarnessError::AgentExecution(format!(
                "container spawn received non-container isolation tier `{}`",
                tier.as_str()
            )));
        }

        let allowlist = network_allowlist(input.env_vars);
        let egress_policy = AgentEgressMode::resolve(input.permission_mode, &allowlist);
        let image = container_image(input.env_vars);
        let review_layout = review_git_safe_workspace(input.env_vars)
            .then(|| review_git::plan(&project_root))
            .transpose()?;
        let workspace_source = review_layout
            .as_ref()
            .map(|layout| layout.workspace_source.as_path())
            .unwrap_or(&project_root);
        let child_workspace = review_layout
            .as_ref()
            .map(|layout| layout.child_workspace.clone())
            .unwrap_or_else(|| PathBuf::from(CONTAINER_WORKSPACE));
        let workspace_read_only = review_layout.is_some()
            || matches!(
                input.sandbox_spec.mode,
                harness_core::config::agents::SandboxMode::ReadOnly
                    | harness_core::config::agents::SandboxMode::ReadOnlyWithNetwork
            );
        let (child_args, output_schema_mount) = output_schema::rewrite_for_container(input.args)?;
        let mut args = vec![OsString::from("run"), OsString::from("--rm")];
        args.push(OsString::from("--name"));
        args.push(OsString::from(unique_resource_name("harness-agent-")));
        append_os_labels(&mut args, ManagedDockerResource::AgentContainer);
        if input.forward_stdin {
            args.push(OsString::from("--interactive"));
        }
        args.push(OsString::from("--workdir"));
        args.push(child_workspace.as_os_str().to_os_string());
        args.push(OsString::from("--mount"));
        let mut workspace_mount = format!(
            "type=bind,src={},dst={CONTAINER_WORKSPACE}",
            workspace_source.display()
        );
        if workspace_read_only {
            workspace_mount.push_str(",readonly");
        }
        args.push(OsString::from(workspace_mount));
        for mount in input.container_bind_mounts {
            let source = match mount.scope {
                ContainerBindMountScope::Workspace => {
                    canonical_container_bind_source(&project_root, &mount.source)?
                }
                ContainerBindMountScope::HarnessTemp => {
                    canonical_harness_temp_bind_source(&mount.source)?
                }
            };
            if !mount.destination.is_absolute() || mount.destination == Path::new("/") {
                return Err(HarnessError::AgentExecution(format!(
                    "container bind destination must be a specific absolute path: {}",
                    mount.destination.display()
                )));
            }
            let mut bind_mount = format!(
                "type=bind,src={},dst={}",
                source.display(),
                mount.destination.display()
            );
            if mount.read_only {
                bind_mount.push_str(",readonly");
            }
            args.push(OsString::from("--mount"));
            args.push(OsString::from(bind_mount));
        }
        if let Some(layout) = &review_layout {
            for mount in &layout.git_mounts {
                args.push(OsString::from("--mount"));
                args.push(OsString::from(format!(
                    "type=bind,src={},dst={},readonly",
                    mount.source.display(),
                    mount.destination.display()
                )));
            }
        }
        if let Some(mount) = &output_schema_mount {
            args.push(OsString::from("--mount"));
            args.push(output_schema::mount_arg(mount));
        }
        args.push(OsString::from("--network"));
        args.push(OsString::from(match egress_policy {
            AgentEgressMode::Unrestricted => "bridge",
            AgentEgressMode::DenyAll => "none",
            AgentEgressMode::FirstPartyProxy => egress_route
                .and_then(EgressProxyRoute::container_network)
                .ok_or_else(|| missing_proxy_route(IsolationTier::Container))?,
        }));
        let ContainerEnv { plain, secret } =
            container_env_vars(input.env_vars, input.secret_env_keys);
        for (key, value) in plain {
            args.push(OsString::from("--env"));
            args.push(OsString::from(format!("{key}={value}")));
        }
        // Secrets are passed by name only. `docker run --env KEY` (no `=value`)
        // reads the value from the Docker client's own environment, keeping the
        // token out of argv and therefore out of the host process list, the
        // tracing arg count/dump, and spawn-failure error strings.
        for key in secret.keys() {
            args.push(OsString::from("--env"));
            args.push(OsString::from(key));
        }
        if let Some(route) = egress_route {
            for key in proxy_env_keys() {
                args.push(OsString::from("--env"));
                args.push(OsString::from(format!("{key}={}", route.proxy_url())));
            }
            for key in ["NO_PROXY", "no_proxy"] {
                args.push(OsString::from("--env"));
                args.push(OsString::from(format!("{key}=localhost,127.0.0.1")));
            }
        }
        if review_git_safe_workspace(input.env_vars) {
            for (key, value) in [
                ("GIT_CONFIG_COUNT", "1"),
                ("GIT_CONFIG_KEY_0", "safe.directory"),
                ("GIT_CONFIG_VALUE_0", CONTAINER_WORKSPACE),
            ] {
                args.push(OsString::from("--env"));
                args.push(OsString::from(format!("{key}={value}")));
            }
            if let Some(layout) = &review_layout {
                for (key, value) in &layout.git_env {
                    args.push(OsString::from("--env"));
                    args.push(OsString::from(format!("{key}={}", value.display())));
                }
            }
        }
        args.push(OsString::from(image));
        if egress_route.is_some_and(EgressProxyRoute::requires_container_canary) {
            let verification_host = allowlist.first().ok_or_else(|| {
                HarnessError::AgentExecution(
                    "first-party egress proxy requires a non-empty allowlist".to_string(),
                )
            })?;
            args.extend(container_canary_command(
                container_program(input.program),
                child_args,
                verification_host,
            ));
        } else {
            args.push(container_program(input.program));
            args.extend(child_args);
        }

        Ok(PreparedAgentSpawn {
            program: PathBuf::from("docker"),
            args,
            current_dir: project_root,
            child_workspace,
            process_env: docker_process_env(secret),
            clear_inherited_env: true,
            sandbox_engine: SandboxEngine::None,
            egress_proxy_lease: None,
            egress_verification: if egress_route.is_some() {
                EgressVerification::AwaitContainerCanary
            } else {
                EgressVerification::NotRequired
            },
        })
    }
}

pub(crate) async fn prepare_agent_spawn(
    input: AgentSpawnInput<'_>,
) -> Result<PreparedAgentSpawn, HarnessError> {
    if input
        .env_vars
        .get(LEGACY_EGRESS_PROXY_ENV)
        .is_some_and(|value| !value.trim().is_empty())
    {
        return Err(HarnessError::AgentExecution(format!(
            "{LEGACY_EGRESS_PROXY_ENV} is no longer accepted because external proxy URLs cannot prove allowlist enforcement; configure {AGENT_EGRESS_PROXY_IMAGE_ENV} instead"
        )));
    }
    let tier = isolation_tier(input.env_vars)?;
    if tier == IsolationTier::Microvm {
        return Err(HarnessError::AgentExecution(
            "isolation tier `microvm` is reserved but not implemented".to_string(),
        ));
    }
    let allowlist = network_allowlist(input.env_vars);
    let egress_policy = AgentEgressMode::resolve(input.permission_mode, &allowlist);
    let lease = if egress_policy == AgentEgressMode::FirstPartyProxy {
        let env_vars = input.env_vars.clone();
        let lease = tokio::task::spawn_blocking(move || {
            EgressProxyLease::start(tier, &allowlist, &env_vars)
        })
        .await
        .map_err(|error| {
            HarnessError::AgentExecution(format!("egress proxy setup task failed: {error}"))
        })??;
        Some(Arc::new(lease))
    } else {
        None
    };
    let route = lease.as_deref().map(EgressProxyLease::route);
    let mut spawn = match tier {
        IsolationTier::Host => HostSpawn.prepare(input, route),
        IsolationTier::Container => ContainerSpawn.prepare(input, route),
        IsolationTier::Microvm => unreachable!("microvm returned before egress setup"),
    }?;
    spawn.egress_proxy_lease = lease;
    Ok(spawn)
}

pub(crate) fn apply_process_env(cmd: &mut tokio::process::Command, spawn: &PreparedAgentSpawn) {
    if spawn.clear_inherited_env {
        cmd.env_clear();
    }
    cmd.envs(spawn.process_env.iter());
    strip_nested_session_env(cmd);
}

/// Remove nested-session markers from the command's *effective* environment.
///
/// `env_remove` overrides both inherited parent env and explicitly set vars,
/// so this works regardless of where the marker came from. Must be applied
/// after all `env`/`envs` calls that could introduce the keys.
pub(crate) fn strip_nested_session_env(cmd: &mut tokio::process::Command) {
    for key in NESTED_SESSION_ENV_KEYS {
        cmd.env_remove(key);
    }
}

fn missing_proxy_route(tier: IsolationTier) -> HarnessError {
    HarnessError::AgentExecution(format!(
        "first-party egress proxy route is missing for `{}` isolation; refusing unrestricted fallback",
        tier.as_str()
    ))
}

fn canonical_workspace(project_root: &Path) -> Result<PathBuf, HarnessError> {
    std::fs::canonicalize(project_root).map_err(|error| {
        HarnessError::AgentExecution(format!(
            "failed to resolve container workspace {}: {error}",
            project_root.display()
        ))
    })
}

fn canonical_container_bind_source(
    project_root: &Path,
    source: &Path,
) -> Result<PathBuf, HarnessError> {
    let source = std::fs::canonicalize(source).map_err(|error| {
        HarnessError::AgentExecution(format!(
            "failed to resolve container bind source {}: {error}",
            source.display()
        ))
    })?;
    if !source.starts_with(project_root) {
        return Err(HarnessError::AgentExecution(format!(
            "container bind source must remain inside the task workspace: {}",
            source.display()
        )));
    }
    Ok(source)
}

fn canonical_harness_temp_bind_source(source: &Path) -> Result<PathBuf, HarnessError> {
    let temp_root = std::fs::canonicalize(std::env::temp_dir()).map_err(|error| {
        HarnessError::AgentExecution(format!("failed to resolve temporary directory: {error}"))
    })?;
    let source = std::fs::canonicalize(source).map_err(|error| {
        HarnessError::AgentExecution(format!(
            "failed to resolve container bind source {}: {error}",
            source.display()
        ))
    })?;
    let trusted = source
        .strip_prefix(&temp_root)
        .ok()
        .and_then(|relative| relative.components().next())
        .is_some_and(|component| {
            component
                .as_os_str()
                .to_string_lossy()
                .starts_with("harness-cloud-setup-")
        });
    if !trusted {
        return Err(HarnessError::AgentExecution(format!(
            "container temporary bind source is not Harness-owned: {}",
            source.display()
        )));
    }
    Ok(source)
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
#[path = "spawn_contract/container_spawn_tests.rs"]
mod container_spawn_tests;

#[cfg(test)]
#[path = "spawn_contract/egress_tests.rs"]
mod egress_tests;
