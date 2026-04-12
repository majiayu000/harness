use crate::thread_manager::ThreadManager;
use harness_agents::registry::{AdapterRegistry, AgentRegistry};
use harness_core::config::{HarnessConfig, ProjectEntry};
use std::net::SocketAddr;
use std::sync::Arc;

pub struct HarnessServer {
    pub config: HarnessConfig,
    pub thread_manager: ThreadManager,
    pub agent_registry: Arc<AgentRegistry>,
    pub adapter_registry: Arc<AdapterRegistry>,
    /// Projects to register in the project registry at startup.
    pub startup_projects: Vec<ProjectEntry>,
    /// The startup project selected as the effective default project root.
    pub startup_default_project: Option<ProjectEntry>,
}

impl HarnessServer {
    pub fn new(
        config: HarnessConfig,
        thread_manager: ThreadManager,
        agent_registry: AgentRegistry,
    ) -> Self {
        Self {
            config,
            thread_manager,
            agent_registry: Arc::new(agent_registry),
            adapter_registry: Arc::new(AdapterRegistry::new("")),
            startup_projects: Vec::new(),
            startup_default_project: None,
        }
    }

    /// Start in stdio mode (JSON-RPC over stdin/stdout).
    pub async fn serve_stdio(self) -> anyhow::Result<()> {
        let state = crate::http::build_app_state(Arc::new(self)).await?;
        crate::stdio::serve(state).await
    }

    /// Start in HTTP + WebSocket mode.
    pub async fn serve_http(self: Arc<Self>, addr: SocketAddr) -> anyhow::Result<()> {
        crate::http::serve(self, addr).await
    }
}
