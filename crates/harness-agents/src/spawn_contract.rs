use harness_core::agent::{AgentEgressMode, AGENT_EGRESS_PROXY_IMAGE_ENV};
#[cfg(test)]
use harness_core::agent::{
    AGENT_CONTAINER_IMAGE_ENV, AGENT_ISOLATION_TIER_ENV, AGENT_NETWORK_ALLOWLIST_ENV,
};
use harness_core::config::agents::AgentPermissionMode;
use harness_core::config::isolation::IsolationTier;
use harness_core::error::HarnessError;
#[cfg(test)]
use harness_core::run_id::AGENT_RUN_ID_ENV;
use harness_sandbox::{wrap_command, NetworkPolicy, SandboxEngine, SandboxSpec};
use std::collections::{BTreeMap, HashMap};
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub(crate) mod egress;
mod output_schema;
mod review_git;
mod spawn_env;
use egress::{
    apply_proxy_env, container_canary_command, proxy_env_keys, EgressProxyLease, EgressProxyRoute,
    LEGACY_EGRESS_PROXY_ENV,
};
use spawn_env::{
    container_env_vars, container_image, docker_process_env, host_process_env, isolation_tier,
    network_allowlist, review_git_safe_workspace, ContainerEnv,
};

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

pub(crate) struct AgentSpawnInput<'a> {
    pub(crate) program: &'a Path,
    pub(crate) args: &'a [OsString],
    pub(crate) project_root: &'a Path,
    pub(crate) sandbox_spec: &'a SandboxSpec,
    pub(crate) env_vars: &'a HashMap<String, String>,
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
        let ContainerEnv { plain, secret } = container_env_vars(input.env_vars);
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
            args.extend(container_canary_command(
                container_program(input.program),
                child_args,
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
            permission_mode: AgentPermissionMode::Scoped,
            forward_stdin: false,
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
        let schema_dir = tempfile::tempdir()?;
        let schema_path = schema_dir.path().join("activity-result-schema.json");
        std::fs::write(&schema_path, "{}")?;
        let args = vec![
            OsString::from("exec"),
            OsString::from("--output-schema"),
            schema_path.as_os_str().to_os_string(),
            OsString::from("prompt"),
        ];
        let sandbox_spec = SandboxSpec::new(SandboxMode::WorkspaceWrite, root.path());

        let spawn = ContainerSpawn.prepare(
            input(
                Path::new("/opt/harness/bin/codex"),
                &args,
                root.path(),
                &sandbox_spec,
                &env_vars,
            ),
            None,
        )?;

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
        assert!(args.contains(&format!(
            "type=bind,src={},dst=/harness-output-schema,readonly",
            std::fs::canonicalize(schema_dir.path())?.display()
        )));
        assert!(args.contains(&"/harness-output-schema/activity-result-schema.json".to_string()));
        assert!(!args.contains(&schema_path.display().to_string()));
        Ok(())
    }

    #[test]
    fn container_spawn_applies_first_party_proxy_route_and_canary() -> anyhow::Result<()> {
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
        let sandbox_spec = SandboxSpec::new(SandboxMode::WorkspaceWrite, root.path());
        let route = EgressProxyRoute::container(
            "harness-egress-test".to_string(),
            "http://egress-proxy:8080".to_string(),
        );

        let spawn = ContainerSpawn.prepare(
            input(
                Path::new("claude"),
                &[],
                root.path(),
                &sandbox_spec,
                &env_vars,
            ),
            Some(&route),
        )?;

        let args = string_args(&spawn);
        assert!(args.contains(&"harness-egress-test".to_string()));
        assert!(args.contains(&"HTTPS_PROXY=http://egress-proxy:8080".to_string()));
        assert!(args.contains(&"all_proxy=http://egress-proxy:8080".to_string()));
        assert!(args
            .iter()
            .any(|arg| arg.contains("canary returned $status")));
        assert!(args.contains(&"claude".to_string()));
        assert!(!args.iter().any(|arg| arg.contains("EGRESS_ALLOWLIST")));
        Ok(())
    }

    #[test]
    fn container_spawn_refuses_allowlist_without_first_party_route() -> anyhow::Result<()> {
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

        let error = ContainerSpawn
            .prepare(
                input(
                    Path::new("codex"),
                    &[],
                    root.path(),
                    &sandbox_spec,
                    &env_vars,
                ),
                None,
            )
            .expect_err("missing first-party route must fail closed");

        assert!(error.to_string().contains("refusing unrestricted fallback"));
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

        let spawn = ContainerSpawn.prepare(
            input(
                Path::new("codex"),
                &[],
                root.path(),
                &sandbox_spec,
                &env_vars,
            ),
            None,
        )?;

        let args = string_args(&spawn);
        assert!(args.contains(&format!("{AGENT_RUN_ID_ENV}=ar-01j00000000000000000000000")));
        // The scoped token reaches the container by name only: `--env KEY`
        // with no `=value`, so no token value is ever rendered into argv.
        assert!(args.contains(&"GITHUB_TOKEN".to_string()));
        assert!(args.contains(&"GH_TOKEN".to_string()));
        assert!(!args.iter().any(|arg| arg.contains("scoped-token")));
        assert!(!args
            .iter()
            .any(|arg| arg.starts_with("HARNESS_SCOPED_GITHUB_TOKEN")));
        assert!(args.contains(&"CARGO_TARGET_DIR=/workspace/target".to_string()));
        assert!(!args.iter().any(|arg| arg.contains("operator-token")));
        assert!(!args.iter().any(|arg| arg.contains("operator-key")));
        // The Docker client process carries the scoped value, and only it.
        assert_eq!(
            spawn.process_env.get("GITHUB_TOKEN"),
            Some(&"scoped-token".to_string())
        );
        assert_eq!(
            spawn.process_env.get("GH_TOKEN"),
            Some(&"scoped-token".to_string())
        );
        assert!(!spawn
            .process_env
            .contains_key("HARNESS_SCOPED_GITHUB_TOKEN"));
        assert!(!spawn.process_env.values().any(|v| v == "operator-token"));
        assert!(spawn.clear_inherited_env);
        Ok(())
    }

    #[test]
    fn container_spawn_keeps_token_values_out_of_every_rendered_string() -> anyhow::Result<()> {
        let root = tempfile::tempdir()?;
        let mut env_vars = HashMap::new();
        env_vars.insert(
            AGENT_ISOLATION_TIER_ENV.to_string(),
            "container".to_string(),
        );
        env_vars.insert(
            "HARNESS_SCOPED_GITHUB_TOKEN".to_string(),
            "ghs_supersecretvalue".to_string(),
        );
        let sandbox_spec = SandboxSpec::new(SandboxMode::WorkspaceWrite, root.path());

        let spawn = ContainerSpawn.prepare(
            input(
                Path::new("codex"),
                &[],
                root.path(),
                &sandbox_spec,
                &env_vars,
            ),
            None,
        )?;

        // Everything an operator or another local user can observe about the
        // launch: the program, the argv, and the Debug rendering used by
        // tracing and error formatting.
        let rendered = format!(
            "{} {:?} {:?}",
            spawn.program.display(),
            spawn.args,
            spawn.args
        );
        assert!(
            !rendered.contains("ghs_supersecretvalue"),
            "token value leaked into the rendered docker command: {rendered}"
        );
        // …while the container still receives it.
        assert_eq!(
            spawn.process_env.get("GITHUB_TOKEN"),
            Some(&"ghs_supersecretvalue".to_string())
        );
        Ok(())
    }

    #[test]
    fn container_spawn_omits_token_env_flags_without_a_scoped_token() -> anyhow::Result<()> {
        let root = tempfile::tempdir()?;
        let mut env_vars = HashMap::new();
        env_vars.insert(
            AGENT_ISOLATION_TIER_ENV.to_string(),
            "container".to_string(),
        );
        let sandbox_spec = SandboxSpec::new(SandboxMode::WorkspaceWrite, root.path());

        let spawn = ContainerSpawn.prepare(
            input(
                Path::new("codex"),
                &[],
                root.path(),
                &sandbox_spec,
                &env_vars,
            ),
            None,
        )?;

        // A bare `--env GITHUB_TOKEN` with nothing in the client environment
        // would be a no-op, but emitting it anyway would imply a credential
        // that does not exist. Nothing is emitted.
        let args = string_args(&spawn);
        assert!(!args.iter().any(|arg| arg == "GITHUB_TOKEN"));
        assert!(!args.iter().any(|arg| arg == "GH_TOKEN"));
        assert!(!spawn.process_env.contains_key("GITHUB_TOKEN"));
        assert!(!spawn.process_env.contains_key("GH_TOKEN"));
        Ok(())
    }

    #[tokio::test]
    async fn host_spawn_filters_injected_nested_session_markers() -> anyhow::Result<()> {
        let root = tempfile::tempdir()?;
        let mut env_vars = HashMap::new();
        for key in NESTED_SESSION_ENV_KEYS {
            env_vars.insert(key.to_string(), "1".to_string());
        }
        env_vars.insert("CLAUDE_CONFIG_DIR".to_string(), "/cfg".to_string());
        let sandbox_spec = SandboxSpec::new(SandboxMode::DangerFullAccess, root.path());

        let spawn = prepare_agent_spawn(input(
            Path::new("claude"),
            &[],
            root.path(),
            &sandbox_spec,
            &env_vars,
        ))
        .await?;

        for key in NESTED_SESSION_ENV_KEYS {
            assert!(
                !spawn.process_env.contains_key(key),
                "{key} must be stripped from the host process env"
            );
        }
        // Legitimate CLAUDE_* configuration must pass through.
        assert_eq!(
            spawn.process_env.get("CLAUDE_CONFIG_DIR"),
            Some(&"/cfg".to_string())
        );
        Ok(())
    }

    #[test]
    fn container_spawn_filters_injected_nested_session_markers() -> anyhow::Result<()> {
        let root = tempfile::tempdir()?;
        let mut env_vars = HashMap::new();
        env_vars.insert(
            AGENT_ISOLATION_TIER_ENV.to_string(),
            "container".to_string(),
        );
        env_vars.insert("CLAUDECODE".to_string(), "1".to_string());
        env_vars.insert("CLAUDE_CODE_ENTRYPOINT".to_string(), "cli".to_string());
        env_vars.insert("CLAUDE_CONFIG_DIR".to_string(), "/cfg".to_string());
        let sandbox_spec = SandboxSpec::new(SandboxMode::WorkspaceWrite, root.path());

        let spawn = ContainerSpawn.prepare(
            input(
                Path::new("claude"),
                &[],
                root.path(),
                &sandbox_spec,
                &env_vars,
            ),
            None,
        )?;

        let args = string_args(&spawn);
        assert!(!args.iter().any(|arg| arg.starts_with("CLAUDECODE=")));
        assert!(!args
            .iter()
            .any(|arg| arg.starts_with("CLAUDE_CODE_ENTRYPOINT=")));
        assert!(args.contains(&"CLAUDE_CONFIG_DIR=/cfg".to_string()));
        Ok(())
    }

    #[tokio::test]
    async fn host_spawn_filters_spawn_control_env() -> anyhow::Result<()> {
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
        ))
        .await?;

        assert!(!spawn.clear_inherited_env);
        assert_eq!(
            spawn.process_env.get("CARGO_TARGET_DIR"),
            Some(&"/tmp/target".to_string())
        );
        assert!(!spawn.process_env.contains_key(AGENT_ISOLATION_TIER_ENV));
        #[cfg(target_os = "macos")]
        {
            assert_eq!(spawn.program, PathBuf::from("/usr/bin/sandbox-exec"));
            assert!(string_args(&spawn)
                .iter()
                .any(|arg| arg.contains("(deny network-outbound)")));
        }
        #[cfg(target_os = "linux")]
        {
            assert_eq!(spawn.program, PathBuf::from("/usr/bin/bwrap"));
            assert!(string_args(&spawn).contains(&"--unshare-net".to_string()));
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "spawn_contract/egress_tests.rs"]
mod egress_tests;
