use crate::thread_manager::ThreadManager;
use harness_agents::registry::AgentRegistry;
use harness_core::config::workflow::load_workflow_document;
use harness_core::config::{HarnessConfig, ProjectEntry};
use std::collections::BTreeSet;
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
    pub retention_max_files: usize,
}

impl RuntimeLogMetadata {
    pub fn disabled(retention_days: u32, retention_max_files: usize) -> Self {
        Self {
            state: RuntimeLogState::Disabled,
            active_path: None,
            path_hint: None,
            retention_days,
            retention_max_files,
        }
    }

    pub fn enabled(active_path: PathBuf, retention_days: u32, retention_max_files: usize) -> Self {
        Self {
            state: RuntimeLogState::Enabled,
            path_hint: Some(Self::public_path_hint(&active_path)),
            active_path: Some(active_path),
            retention_days,
            retention_max_files,
        }
    }

    pub fn degraded(
        path_hint: Option<String>,
        retention_days: u32,
        retention_max_files: usize,
    ) -> Self {
        Self {
            state: RuntimeLogState::Degraded,
            active_path: None,
            path_hint,
            retention_days,
            retention_max_files,
        }
    }

    pub fn public_path_hint(path: &Path) -> String {
        path.to_string_lossy().into_owned()
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
        #[cfg(test)]
        crate::test_helpers::configure_test_pg_pool_defaults();
        let retention_days = config.observe.log_retention_days;
        let retention_max_files = config.observe.log_retention_max_files;
        Self {
            config,
            thread_manager,
            agent_registry: Arc::new(agent_registry),
            runtime_logs: RuntimeLogMetadata::disabled(retention_days, retention_max_files),
            startup_projects: Vec::new(),
            startup_default_project: None,
        }
    }

    /// Start in stdio mode (JSON-RPC over stdin/stdout).
    pub async fn serve_stdio(self) -> anyhow::Result<()> {
        self.register_declarative_workflow_definitions()?;
        harness_workflow::runtime::freeze_workflow_definition_registry();
        let state = crate::http::build_app_state(Arc::new(self)).await?;
        crate::stdio::serve(state).await
    }

    /// Start in HTTP + WebSocket mode.
    pub async fn serve_http(self: Arc<Self>, addr: SocketAddr) -> anyhow::Result<()> {
        self.register_declarative_workflow_definitions()?;
        harness_workflow::runtime::freeze_workflow_definition_registry();
        crate::http::serve(self, addr).await
    }

    fn register_declarative_workflow_definitions(&self) -> anyhow::Result<()> {
        harness_workflow::runtime::register_declarative_workflow_definitions(
            self.load_declarative_workflow_definitions()?,
        )
    }

    fn load_declarative_workflow_definitions(
        &self,
    ) -> anyhow::Result<Vec<harness_workflow::runtime::DeclarativeWorkflowDefinition>> {
        let mut project_roots = BTreeSet::new();
        project_roots.insert(project_root_identity(&self.config.server.project_root));
        for project in self
            .startup_projects
            .iter()
            .chain(self.startup_default_project.iter())
        {
            project_roots.insert(project_root_identity(&project.root));
        }

        let mut definitions = Vec::new();
        for project_root in project_roots {
            let document = load_workflow_document(&project_root).map_err(|error| {
                anyhow::anyhow!(
                    "failed to load workflow definition for project '{}': {error}",
                    project_root.display()
                )
            })?;
            let Some(policy) = document.config.definition.as_ref() else {
                continue;
            };
            let definition = harness_workflow::runtime::build_declarative_definition(
                policy,
                &document.config.activities,
            )
            .map_err(|error| {
                anyhow::anyhow!(
                    "invalid workflow definition '{}' for project '{}': {error}",
                    policy.id,
                    project_root.display()
                )
            })?;
            definitions.push(definition);
        }
        Ok(definitions)
    }
}

fn project_root_identity(root: &Path) -> PathBuf {
    std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enabled_runtime_log_metadata_uses_active_path_as_hint() {
        let active_path = PathBuf::from(
            "/tmp/harness-codex-runtime/logs/harness-serve-20260430T120000Z-pid1.log",
        );
        let metadata = RuntimeLogMetadata::enabled(active_path.clone(), 30, 30);

        assert_eq!(metadata.active_path.as_deref(), Some(active_path.as_path()));
        assert_eq!(
            metadata.path_hint.as_deref(),
            Some(active_path.to_string_lossy().as_ref())
        );
        assert_eq!(metadata.retention_max_files, 30);
    }

    #[test]
    fn declarative_definitions_compile_as_an_atomic_startup_batch() -> anyhow::Result<()> {
        let project = tempfile::tempdir()?;
        std::fs::write(
            project.path().join("WORKFLOW.md"),
            r#"---
definition:
  id: startup_docs_review
  initial: review
  states:
    review:
      activity: review_docs
      on_success: done
      on_failure: failed
      on_blocked: blocked
      on_signal: { cancel: cancelled }
    blocked: { progress: operator_gate }
  terminal: { done: succeeded, failed: failed, cancelled: cancelled }
activities:
  review_docs: { prompt: review }
---
Review documentation.
"#,
        )?;
        let mut config = HarnessConfig::default();
        config.server.project_root = project.path().to_path_buf();
        let server = HarnessServer::new(config, ThreadManager::new(), AgentRegistry::new("test"));

        let definitions = server.load_declarative_workflow_definitions()?;
        assert_eq!(definitions.len(), 1);
        assert_eq!(definitions[0].policy().id, "startup_docs_review");

        let mut registry = harness_workflow::runtime::WorkflowDefinitionRegistry::new();
        registry.register_declarative_current_batch(definitions)?;
        registry.freeze();
        assert!(registry.is_frozen());
        Ok(())
    }
}
