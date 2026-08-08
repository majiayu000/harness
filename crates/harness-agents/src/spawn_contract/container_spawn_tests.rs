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
        secret_env_keys: &[],
        container_bind_mounts: &[],
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

fn policy_request() -> TurnRequest {
    TurnRequest {
        prompt: "first prompt".to_string(),
        prompt_layers: None,
        project_root: PathBuf::from("/tmp/project"),
        permission_mode: AgentPermissionMode::Full,
        model: Some("model-a".to_string()),
        reasoning_effort: None,
        execution_phase: None,
        sandbox_mode: Some(SandboxMode::DangerFullAccess),
        approval_policy: None,
        allowed_tools: None,
        context: Vec::new(),
        timeout_secs: Some(30),
        env_vars: HashMap::new(),
        capability_token: None,
    }
}

#[test]
fn adapter_spawn_policy_changes_with_process_security_controls() {
    let request = policy_request();
    let baseline = adapter_spawn_policy_fingerprint(&request, SandboxMode::ReadOnly);

    let mut scoped = request.clone();
    scoped.permission_mode = AgentPermissionMode::Scoped;
    assert!(baseline != adapter_spawn_policy_fingerprint(&scoped, SandboxMode::ReadOnly));

    let mut allowlisted = request.clone();
    allowlisted.env_vars.insert(
        AGENT_NETWORK_ALLOWLIST_ENV.to_string(),
        "api.openai.com".to_string(),
    );
    assert!(baseline != adapter_spawn_policy_fingerprint(&allowlisted, SandboxMode::ReadOnly));

    let mut sandboxed = request.clone();
    sandboxed.sandbox_mode = Some(SandboxMode::WorkspaceWrite);
    assert!(baseline != adapter_spawn_policy_fingerprint(&sandboxed, SandboxMode::ReadOnly));
}

#[test]
fn adapter_spawn_policy_ignores_turn_only_fields() {
    let request = policy_request();
    let baseline = adapter_spawn_policy_fingerprint(&request, SandboxMode::ReadOnly);
    let mut next_turn = request;
    next_turn.prompt = "second prompt".to_string();
    next_turn.model = Some("model-b".to_string());
    next_turn.timeout_secs = Some(90);
    next_turn.env_vars.insert(
        AGENT_RUN_ID_ENV.to_string(),
        "ar-01j00000000000000000000000".to_string(),
    );
    next_turn.env_vars.insert(
        AGENT_RUN_PARENT_ENV.to_string(),
        "ar-01j00000000000000000000001".to_string(),
    );

    assert!(baseline == adapter_spawn_policy_fingerprint(&next_turn, SandboxMode::ReadOnly));
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
fn container_spawn_adds_workspace_scoped_state_mounts() -> anyhow::Result<()> {
    let root = tempfile::tempdir()?;
    let state_home = root.path().join(".harness/cloud-setup-state/test/home");
    std::fs::create_dir_all(&state_home)?;
    let env_vars = HashMap::from([(
        AGENT_ISOLATION_TIER_ENV.to_string(),
        "container".to_string(),
    )]);
    let sandbox_spec = SandboxSpec::new(SandboxMode::ReadOnly, root.path());
    let mounts = [ContainerBindMount::workspace(
        state_home.clone(),
        PathBuf::from("/harness-cloud-home"),
    )];

    let spawn = ContainerSpawn.prepare(
        AgentSpawnInput {
            program: Path::new("codex"),
            args: &[],
            project_root: root.path(),
            sandbox_spec: &sandbox_spec,
            env_vars: &env_vars,
            secret_env_keys: &[],
            container_bind_mounts: &mounts,
            permission_mode: AgentPermissionMode::Scoped,
            forward_stdin: false,
        },
        None,
    )?;

    assert!(string_args(&spawn).contains(&format!(
        "type=bind,src={},dst=/harness-cloud-home",
        std::fs::canonicalize(state_home)?.display()
    )));
    Ok(())
}

#[test]
fn container_spawn_rejects_state_mount_outside_workspace() -> anyhow::Result<()> {
    let root = tempfile::tempdir()?;
    let outside = tempfile::tempdir()?;
    let env_vars = HashMap::from([(
        AGENT_ISOLATION_TIER_ENV.to_string(),
        "container".to_string(),
    )]);
    let sandbox_spec = SandboxSpec::new(SandboxMode::ReadOnly, root.path());
    let mounts = [ContainerBindMount::workspace(
        outside.path().to_path_buf(),
        PathBuf::from("/harness-cloud-home"),
    )];

    let error = ContainerSpawn
        .prepare(
            AgentSpawnInput {
                program: Path::new("codex"),
                args: &[],
                project_root: root.path(),
                sandbox_spec: &sandbox_spec,
                env_vars: &env_vars,
                secret_env_keys: &[],
                container_bind_mounts: &mounts,
                permission_mode: AgentPermissionMode::Scoped,
                forward_stdin: false,
            },
            None,
        )
        .expect_err("state mount outside the task workspace must fail closed");

    assert!(error
        .to_string()
        .contains("container bind source must remain inside the task workspace"));
    Ok(())
}

#[test]
fn container_spawn_accepts_harness_owned_temporary_mount() -> anyhow::Result<()> {
    let root = tempfile::tempdir()?;
    let temporary = tempfile::Builder::new()
        .prefix("harness-cloud-setup-")
        .tempdir()?;
    let home = temporary.path().join("home");
    std::fs::create_dir(&home)?;
    let env_vars = HashMap::from([(
        AGENT_ISOLATION_TIER_ENV.to_string(),
        "container".to_string(),
    )]);
    let sandbox_spec = SandboxSpec::new(SandboxMode::ReadOnly, root.path());
    let mounts = [ContainerBindMount::harness_temp(
        home.clone(),
        PathBuf::from("/harness-cloud-home"),
    )];

    let spawn = ContainerSpawn.prepare(
        AgentSpawnInput {
            program: Path::new("codex"),
            args: &[],
            project_root: root.path(),
            sandbox_spec: &sandbox_spec,
            env_vars: &env_vars,
            secret_env_keys: &[],
            container_bind_mounts: &mounts,
            permission_mode: AgentPermissionMode::Scoped,
            forward_stdin: false,
        },
        None,
    )?;

    assert!(string_args(&spawn).contains(&format!(
        "type=bind,src={},dst=/harness-cloud-home",
        std::fs::canonicalize(home)?.display()
    )));
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
    assert!(args
        .iter()
        .any(|arg| arg.contains("could not reach allowlisted host")));
    assert!(args.contains(&"github.com".to_string()));
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
