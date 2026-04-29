use crate::thread_manager::ThreadManager;
use harness_agents::registry::AgentRegistry;
use harness_core::config::{HarnessConfig, ProjectEntry};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeLogState {
    Disabled,
    Enabled,
    Degraded,
}

impl RuntimeLogState {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::Enabled => "enabled",
            Self::Degraded => "degraded",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeLogMetadata {
    pub state: RuntimeLogState,
    pub active_path: Option<PathBuf>,
    pub path_hint: Option<String>,
    pub retention_days: u32,
}

impl RuntimeLogMetadata {
    pub fn disabled(retention_days: u32) -> Self {
        Self {
            state: RuntimeLogState::Disabled,
            active_path: None,
            path_hint: None,
            retention_days,
        }
    }

    pub fn enabled(active_path: PathBuf, retention_days: u32) -> Self {
        Self {
            state: RuntimeLogState::Enabled,
            path_hint: Some(Self::public_path_hint(&active_path)),
            active_path: Some(active_path),
            retention_days,
        }
    }

    pub fn degraded(path_hint: Option<String>, retention_days: u32) -> Self {
        Self {
            state: RuntimeLogState::Degraded,
            active_path: None,
            path_hint,
            retention_days,
        }
    }

    pub fn public_path_hint(path: &Path) -> String {
        path.file_name()
            .map(|name| format!("logs/{}", name.to_string_lossy()))
            .unwrap_or_else(|| "logs".to_string())
    }
}

pub struct HarnessServer {
    pub config: HarnessConfig,
    pub thread_manager: ThreadManager,
    pub agent_registry: Arc<AgentRegistry>,
    pub runtime_logs: RuntimeLogMetadata,
    /// Projects to register in the project registry at startup.
    pub startup_projects: Vec<ProjectEntry>,
    /// The startup project selected as the effective default project root.
    pub startup_default_project: Option<ProjectEntry>,
}

impl HarnessServer {
    pub fn new(
        mut config: HarnessConfig,
        thread_manager: ThreadManager,
        agent_registry: AgentRegistry,
    ) -> Self {
        config.apply_derived_defaults();
        let retention_days = config.observe.log_retention_days;
        Self {
            config,
            thread_manager,
            agent_registry: Arc::new(agent_registry),
            runtime_logs: RuntimeLogMetadata::disabled(retention_days),
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
