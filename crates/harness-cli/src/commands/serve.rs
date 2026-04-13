use anyhow::Result;
use harness_core::config::HarnessConfig;
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

fn startup_default_project_for_root(
    parsed_projects: &[harness_core::config::ProjectEntry],
    default_project_id: Option<&str>,
    project_root: &std::path::Path,
) -> Option<harness_core::config::ProjectEntry> {
    let canonical_project_root = project_root.canonicalize().ok();
    default_project_id.and_then(|id| {
        parsed_projects
            .iter()
            .find(|project| project.name == id)
            .filter(|project| {
                canonical_project_root
                    .as_ref()
                    .and_then(|canonical_root| {
                        project
                            .root
                            .canonicalize()
                            .ok()
                            .map(|root| root == *canonical_root)
                    })
                    .unwrap_or_else(|| project.root == project_root)
            })
            .cloned()
    })
}

pub async fn run(
    mut config: HarnessConfig,
    transport: Option<String>,
    port: Option<u16>,
    project_root: Option<PathBuf>,
    projects: Vec<String>,
    default_project: Option<String>,
) -> Result<()> {
    // Apply env var overrides before CLI flags so CLI always wins.
    config.apply_env_overrides()?;

    // On macOS, sandbox-exec (Seatbelt) with deny-default policy causes SIGTRAP
    // in Claude Code CLI. Only enforce danger-full-access when a Claude path
    // is actually configured to run.
    let claude_in_use = config.agents.default_agent == "claude"
        || config.agents.default_agent.eq_ignore_ascii_case("auto")
        || config.review.agent.as_deref() == Some("claude")
        || config.agents.review.reviewer_agent == "claude";
    if cfg!(target_os = "macos")
        && claude_in_use
        && config.agents.sandbox_mode != harness_core::config::agents::SandboxMode::DangerFullAccess
    {
        anyhow::bail!(
            "sandbox_mode = {:?} is not supported on macOS — \
             Claude Code CLI requires syscalls blocked by Seatbelt. \
             Set agents.sandbox_mode = \"danger-full-access\" in config.",
            config.agents.sandbox_mode
        );
    }

    // Collect projects from config file `[[projects]]` entries.
    let mut parsed_projects: Vec<harness_core::config::ProjectEntry> = Vec::new();
    let mut config_default_name: Option<String> = None;
    for entry in &config.projects {
        if entry.name.is_empty() {
            anyhow::bail!("[[projects]] entry has empty name");
        }
        if entry.name == "default" {
            anyhow::bail!("[[projects]] name 'default' is reserved; choose a different name");
        }
        if parsed_projects
            .iter()
            .any(|project| project.name == entry.name)
        {
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
        let mut project = entry.clone();
        project.root = canonical;
        parsed_projects.push(project);
    }

    // Merge CLI --project flags (override config entries with same name).
    for raw in &projects {
        let (name, path) = parse_project_entry(raw)?;
        let existing = parsed_projects
            .iter()
            .find(|project| project.name == name)
            .cloned();
        parsed_projects.retain(|project| project.name != name);
        let canonical = path.canonicalize().map_err(|e| {
            anyhow::anyhow!(
                "--project {name}: path '{}' is not accessible: {e}",
                path.display()
            )
        })?;
        if let Err(reason) = validate_project_root(&canonical) {
            anyhow::bail!("--project {name}: {reason}");
        }
        let mut project = existing.unwrap_or(harness_core::config::ProjectEntry {
            name,
            root: canonical.clone(),
            default: false,
            default_agent: None,
            max_concurrent: None,
        });
        project.root = canonical;
        parsed_projects.push(project);
    }

    // Determine the default project id: CLI --default-project > config `default = true` > first entry.
    let default_project_id: Option<String> = if !parsed_projects.is_empty() {
        let id = default_project
            .clone()
            .or(config_default_name)
            .unwrap_or_else(|| parsed_projects[0].name.clone());
        if !parsed_projects.iter().any(|project| project.name == id) {
            let known: Vec<&str> = parsed_projects
                .iter()
                .map(|project| project.name.as_str())
                .collect();
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
        if let Some(project) = parsed_projects.iter().find(|project| &project.name == id) {
            serve_config.server.project_root = project.root.clone();
        }
    }
    if let Some(project_root) = project_root {
        serve_config.server.project_root = project_root;
    }

    let startup_default_project = startup_default_project_for_root(
        &parsed_projects,
        default_project_id.as_deref(),
        &serve_config.server.project_root,
    );

    let thread_manager = harness_server::thread_manager::ThreadManager::new();
    let mut agent_registry =
        harness_agents::registry::AgentRegistry::new(&serve_config.agents.default_agent);
    agent_registry
        .set_complexity_preferences(serve_config.agents.complexity_preferred_agents.clone());
    let mut claude_agent = harness_agents::claude::ClaudeCodeAgent::new(
        serve_config.agents.claude.cli_path.clone(),
        serve_config.agents.claude.default_model.clone(),
        serve_config.agents.sandbox_mode,
    )
    .with_no_session_persistence_probe();
    if let Some(budget) = serve_config.agents.claude.reasoning_budget.clone() {
        claude_agent = claude_agent.with_reasoning_budget(budget);
    }
    claude_agent = claude_agent.with_stream_timeout(serve_config.agents.stream_timeout_secs);
    agent_registry.register("claude", Arc::new(claude_agent));
    agent_registry.register(
        "codex",
        Arc::new(
            harness_agents::codex::CodexAgent::from_config(
                serve_config.agents.codex.clone(),
                serve_config.agents.sandbox_mode,
            )
            .with_stream_timeout(serve_config.agents.stream_timeout_secs),
        ),
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
    server.startup_default_project = startup_default_project;

    let effective_transport = match transport.as_deref() {
        Some("stdio") => harness_core::config::server::Transport::Stdio,
        Some("http") => harness_core::config::server::Transport::Http,
        Some("web_socket") => harness_core::config::server::Transport::WebSocket,
        Some(other) => anyhow::bail!("unknown transport: {other}"),
        None => serve_config.server.transport,
    };

    match effective_transport {
        harness_core::config::server::Transport::Stdio => server.serve_stdio().await?,
        harness_core::config::server::Transport::Http
        | harness_core::config::server::Transport::WebSocket => {
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

#[cfg(test)]
mod tests {
    use super::startup_default_project_for_root;
    use harness_core::config::ProjectEntry;
    use std::path::Path;

    #[test]
    fn startup_default_project_ignores_metadata_after_project_root_override() {
        let project = ProjectEntry {
            name: "repo-a".to_string(),
            root: Path::new("/repo-a").to_path_buf(),
            default: true,
            default_agent: Some("claude".to_string()),
            max_concurrent: Some(3),
        };

        let startup_default =
            startup_default_project_for_root(&[project], Some("repo-a"), Path::new("/repo-b"));

        assert!(startup_default.is_none());
    }

    #[test]
    fn startup_default_project_matches_same_root_via_relative_path_alias() {
        let canonical_root = std::env::current_dir()
            .expect("cwd")
            .canonicalize()
            .expect("canonical root");
        let project = ProjectEntry {
            name: "repo-a".to_string(),
            root: canonical_root,
            default: true,
            default_agent: Some("claude".to_string()),
            max_concurrent: Some(3),
        };

        let startup_default =
            startup_default_project_for_root(&[project], Some("repo-a"), Path::new("."));

        assert_eq!(
            startup_default.map(|project| project.name),
            Some("repo-a".to_string())
        );
    }
}
