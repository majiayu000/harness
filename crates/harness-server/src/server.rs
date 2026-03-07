use crate::thread_manager::ThreadManager;
use harness_agents::AgentRegistry;
use harness_core::HarnessConfig;
use std::net::SocketAddr;
use std::sync::Arc;

pub struct HarnessServer {
    pub config: HarnessConfig,
    pub thread_manager: ThreadManager,
    pub agent_registry: Arc<AgentRegistry>,
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
