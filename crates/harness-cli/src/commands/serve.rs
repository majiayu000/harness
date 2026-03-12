use anyhow::Result;
use harness_core::HarnessConfig;
use std::path::PathBuf;
use std::sync::Arc;

pub async fn run(
    config: HarnessConfig,
    transport: String,
    port: Option<u16>,
    project_root: Option<PathBuf>,
) -> Result<()> {
    let mut serve_config = config.clone();
    if let Some(project_root) = project_root {
        serve_config.server.project_root = project_root;
    }
    let thread_manager = harness_server::thread_manager::ThreadManager::new();
    let mut agent_registry = harness_agents::AgentRegistry::new(&serve_config.agents.default_agent);
    agent_registry.register(
        "claude",
        Arc::new(harness_agents::claude::ClaudeCodeAgent::new(
            serve_config.agents.claude.cli_path.clone(),
            serve_config.agents.claude.default_model.clone(),
            serve_config.agents.sandbox_mode,
        )),
    );
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
    let server = harness_server::server::HarnessServer::new(
        serve_config.clone(),
        thread_manager,
        agent_registry,
    );

    match transport.as_str() {
        "stdio" => server.serve_stdio().await?,
        "http" => {
            let addr = if let Some(p) = port {
                format!("127.0.0.1:{p}").parse()?
            } else {
                serve_config.server.http_addr
            };
            Arc::new(server).serve_http(addr).await?
        }
        other => anyhow::bail!("unknown transport: {other}"),
    }

    Ok(())
}
