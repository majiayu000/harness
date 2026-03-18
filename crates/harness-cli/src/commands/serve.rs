use anyhow::Result;
use harness_core::HarnessConfig;
use harness_server::project_registry::validate_project_root;
use std::path::PathBuf;
use std::sync::Arc;

/// Parse a `name=path` string into `(name, PathBuf)`.
fn parse_project_entry(s: &str) -> Result<(String, PathBuf)> {
    let (name, path) = s
        .split_once('=')
        .ok_or_else(|| anyhow::anyhow!("--project must be in NAME=PATH format, got: {s:?}"))?;
    if name.is_empty() {
        anyhow::bail!("--project name cannot be empty in: {s:?}");
    }
    if name == "default" {
        anyhow::bail!("--project name 'default' is reserved; choose a different name in: {s:?}");
    }
    if path.is_empty() {
        anyhow::bail!("--project path cannot be empty in: {s:?}");
    }
    Ok((name.to_string(), PathBuf::from(path)))
}

pub async fn run(
    config: HarnessConfig,
    transport: Option<String>,
    port: Option<u16>,
    project_root: Option<PathBuf>,
    projects: Vec<String>,
    default_project: Option<String>,
) -> Result<()> {
    // On macOS, sandbox-exec (Seatbelt) with deny-default policy causes SIGTRAP
    // in Claude Code CLI. Require danger-full-access until Seatbelt policy is
    // expanded to cover all syscalls Claude Code needs.
    if cfg!(target_os = "macos")
        && config.agents.sandbox_mode != harness_core::SandboxMode::DangerFullAccess
    {
        anyhow::bail!(
            "sandbox_mode = {:?} is not supported on macOS — \
             Claude Code CLI requires syscalls blocked by Seatbelt. \
             Set agents.sandbox_mode = \"danger-full-access\" in config.",
            config.agents.sandbox_mode
        );
    }

    // Collect projects from config file `[[projects]]` entries.
    let mut parsed_projects: Vec<(String, PathBuf)> = Vec::new();
    let mut config_default_name: Option<String> = None;
    for entry in &config.projects {
        if entry.name.is_empty() {
            anyhow::bail!("[[projects]] entry has empty name");
        }
        if entry.name == "default" {
            anyhow::bail!("[[projects]] name 'default' is reserved; choose a different name");
        }
        if parsed_projects.iter().any(|(n, _)| n == &entry.name) {
            anyhow::bail!(
                "[[projects]] name '{}' is duplicated; each project name must be unique",
                entry.name
            );
        }
        let canonical = entry.root.canonicalize().map_err(|e| {
            anyhow::anyhow!(
                "[[projects]] {}: path '{}' is not accessible: {e}",
                entry.name,
                entry.root.display()
            )
        })?;
        if let Err(reason) = validate_project_root(&canonical) {
            anyhow::bail!("[[projects]] {}: {reason}", entry.name);
        }
        if entry.default {
            config_default_name = Some(entry.name.clone());
        }
        parsed_projects.push((entry.name.clone(), canonical));
    }

    // Merge CLI --project flags (override config entries with same name).
    for raw in &projects {
        let (name, path) = parse_project_entry(raw)?;
        if parsed_projects.iter().any(|(n, _)| n == &name) {
            parsed_projects.retain(|(n, _)| n != &name);
        }
        let canonical = path.canonicalize().map_err(|e| {
            anyhow::anyhow!(
                "--project {name}: path '{}' is not accessible: {e}",
                path.display()
            )
        })?;
        if let Err(reason) = validate_project_root(&canonical) {
            anyhow::bail!("--project {name}: {reason}");
        }
        parsed_projects.push((name, canonical));
    }

    // Determine the default project id: CLI --default-project > config `default = true` > first entry.
    let default_project_id: Option<String> = if !parsed_projects.is_empty() {
        let id = default_project
            .clone()
            .or(config_default_name)
            .unwrap_or_else(|| parsed_projects[0].0.clone());
        if !parsed_projects.iter().any(|(n, _)| n == &id) {
            let known: Vec<&str> = parsed_projects.iter().map(|(n, _)| n.as_str()).collect();
            anyhow::bail!(
                "default project {id:?} does not match any project name; known names: {known:?}"
            );
        }
        Some(id)
    } else {
        None
    };

    let mut serve_config = config.clone();
    // When --project entries are provided, set project_root to the default project's path
    // so existing single-project behaviour is preserved.
    if let Some(ref id) = default_project_id {
        if let Some((_, path)) = parsed_projects.iter().find(|(n, _)| n == id) {
            serve_config.server.project_root = path.clone();
        }
    }
    if let Some(project_root) = project_root {
        serve_config.server.project_root = project_root;
    }

    let thread_manager = harness_server::thread_manager::ThreadManager::new();
    let mut agent_registry = harness_agents::AgentRegistry::new(&serve_config.agents.default_agent);
    let mut claude_agent = harness_agents::claude::ClaudeCodeAgent::new(
        serve_config.agents.claude.cli_path.clone(),
        serve_config.agents.claude.default_model.clone(),
        serve_config.agents.sandbox_mode,
    );
    if let Some(budget) = serve_config.agents.claude.reasoning_budget.clone() {
        claude_agent = claude_agent.with_reasoning_budget(budget);
    }
    agent_registry.register("claude", Arc::new(claude_agent));
    agent_registry.register(
        "codex",
        Arc::new(harness_agents::codex::CodexAgent::from_config(
            serve_config.agents.codex.clone(),
            serve_config.agents.sandbox_mode,
        )),
    );
    if let Ok(api_key) = std::env::var("ANTHROPIC_API_KEY") {
        agent_registry.register(
            "anthropic-api",
            Arc::new(
                harness_agents::anthropic_api::AnthropicApiAgent::from_config(
                    api_key,
                    &serve_config.agents.anthropic_api,
                ),
            ),
        );
    }
    let mut server = harness_server::server::HarnessServer::new(
        serve_config.clone(),
        thread_manager,
        agent_registry,
    );
    server.startup_projects = parsed_projects;

    let effective_transport = match transport.as_deref() {
        Some("stdio") => harness_core::Transport::Stdio,
        Some("http") => harness_core::Transport::Http,
        Some("web_socket") => harness_core::Transport::WebSocket,
        Some(other) => anyhow::bail!("unknown transport: {other}"),
        None => serve_config.server.transport,
    };

    match effective_transport {
        harness_core::Transport::Stdio => server.serve_stdio().await?,
        harness_core::Transport::Http | harness_core::Transport::WebSocket => {
            let addr = if let Some(p) = port {
                format!("127.0.0.1:{p}").parse()?
            } else {
                serve_config.server.http_addr
            };
            Arc::new(server).serve_http(addr).await?
        }
    }

    Ok(())
}
