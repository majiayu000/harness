pub mod agents;
pub mod dirs;
pub mod intake;
pub mod maintenance;
pub mod misc;
pub mod project;
pub mod resolve;
pub mod server;
pub mod shutdown;
pub mod workflow;
pub mod workflow_circuit_breaker;

use self::agents::*;
use self::intake::*;
use self::maintenance::*;
use self::misc::*;
use self::server::*;
use self::workflow_circuit_breaker::*;
use serde::{Deserialize, Deserializer, Serialize};
use std::path::PathBuf;

/// A project entry declared in the config file under `[[projects]]`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectEntry {
    /// Unique project name used as identifier in APIs and logs.
    pub name: String,
    /// Absolute or relative path to the project root (must be a git repo).
    pub root: PathBuf,
    /// Mark this project as the default for single-project API calls.
    #[serde(default)]
    pub default: bool,
    /// Override the default agent for this project.
    #[serde(default)]
    pub default_agent: Option<String>,
    /// Maximum concurrent tasks for this project.
    #[serde(default)]
    pub max_concurrent: Option<u32>,
}

#[derive(Debug, Clone, Serialize)]
pub struct HarnessConfig {
    pub server: ServerConfig,
    pub agents: AgentsConfig,
    pub gc: GcConfig,
    pub rules: RulesConfig,
    pub observe: ObserveConfig,
    #[serde(default)]
    pub context: ContextConfig,
    pub otel: OtelConfig,
    #[serde(default)]
    pub validation: ValidationConfig,
    #[serde(default)]
    pub workspace: WorkspaceConfig,
    #[serde(default)]
    pub concurrency: ConcurrencyConfig,
    #[serde(default)]
    pub intake: IntakeConfig,
    #[serde(default)]
    pub review: ReviewConfig,
    #[serde(default)]
    pub retry_scheduler: RetrySchedulerConfig,
    #[serde(default)]
    pub reconciliation: ReconciliationConfig,
    #[serde(default)]
    pub maintenance_window: MaintenanceWindowConfig,
    #[serde(default)]
    pub workflow: WorkflowRuntimeConfig,
    /// Projects declared in the config file. Registered on server startup.
    #[serde(default)]
    pub projects: Vec<ProjectEntry>,
}

impl HarnessConfig {
    /// Apply environment variable overrides to all sub-configs.
    ///
    /// Call this after loading from a TOML file (or using `Default`) and
    /// before applying CLI flags so that CLI flags retain the highest
    /// precedence.
    pub fn apply_env_overrides(&mut self) -> anyhow::Result<()> {
        let workspace_root_is_implicit = !self.workspace.root_configured;
        self.server.apply_env_overrides()?;
        if workspace_root_is_implicit {
            self.workspace.root = WorkspaceConfig::root_for_data_dir(&self.server.data_dir);
        } else {
            self.apply_derived_defaults();
        }
        Ok(())
    }

    /// Apply defaults that depend on other config fields.
    ///
    /// Serde field defaults cannot see sibling fields, so cross-field defaults
    /// are resolved after loading, rebasing, and environment overrides.
    pub fn apply_derived_defaults(&mut self) {
        self.server.normalize_api_token();
        self.workspace
            .use_data_dir_default_root(&self.server.data_dir);
    }

    /// Rebase all relative `PathBuf` fields against `base`.
    ///
    /// Call this immediately after loading a config from a discovered file so
    /// that relative paths (e.g. `project_root = "."`) resolve against the
    /// config file's parent directory rather than the process working directory.
    /// Paths that are already absolute are left unchanged.
    pub fn rebase_relative_paths(&mut self, base: &std::path::Path) {
        fn rebase(path: &mut PathBuf, base: &std::path::Path) {
            if path.is_relative() {
                *path = base.join(&*path);
            }
        }
        fn rebase_opt(path: &mut Option<PathBuf>, base: &std::path::Path) {
            if let Some(p) = path {
                if p.is_relative() {
                    *p = base.join(&*p);
                }
            }
        }
        rebase(&mut self.server.data_dir, base);
        rebase(&mut self.server.project_root, base);
        for p in &mut self.server.allowed_project_roots {
            rebase(p, base);
        }
        for entry in &mut self.projects {
            rebase(&mut entry.root, base);
        }
        for p in &mut self.rules.discovery_paths {
            rebase(p, base);
        }
        rebase_opt(&mut self.rules.builtin_path, base);
        for p in &mut self.rules.exec_policy_paths {
            rebase(p, base);
        }
        rebase_opt(&mut self.rules.requirements_path, base);
        if self.workspace.root_configured {
            rebase(&mut self.workspace.root, base);
        }
        self.apply_derived_defaults();
    }
}

impl Default for HarnessConfig {
    fn default() -> Self {
        let mut config = Self {
            server: ServerConfig::default(),
            agents: AgentsConfig::default(),
            gc: GcConfig::default(),
            rules: RulesConfig::default(),
            observe: ObserveConfig::default(),
            context: ContextConfig::default(),
            otel: OtelConfig::default(),
            validation: ValidationConfig::default(),
            workspace: WorkspaceConfig::default(),
            concurrency: ConcurrencyConfig::default(),
            intake: IntakeConfig::default(),
            review: ReviewConfig::default(),
            retry_scheduler: RetrySchedulerConfig::default(),
            reconciliation: ReconciliationConfig::default(),
            maintenance_window: MaintenanceWindowConfig::default(),
            workflow: WorkflowRuntimeConfig::default(),
            projects: Vec::new(),
        };
        config.apply_derived_defaults();
        config
    }
}

impl<'de> Deserialize<'de> for HarnessConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct HarnessConfigInput {
            server: ServerConfig,
            agents: AgentsConfig,
            gc: GcConfig,
            rules: RulesConfig,
            observe: ObserveConfig,
            #[serde(default)]
            context: ContextConfig,
            otel: OtelConfig,
            #[serde(default)]
            validation: ValidationConfig,
            #[serde(default)]
            workspace: WorkspaceConfig,
            #[serde(default)]
            concurrency: ConcurrencyConfig,
            #[serde(default)]
            intake: IntakeConfig,
            #[serde(default)]
            review: ReviewConfig,
            #[serde(default)]
            retry_scheduler: RetrySchedulerConfig,
            #[serde(default)]
            reconciliation: ReconciliationConfig,
            #[serde(default)]
            maintenance_window: MaintenanceWindowConfig,
            #[serde(default)]
            workflow: WorkflowRuntimeConfig,
            #[serde(default)]
            projects: Vec<ProjectEntry>,
        }

        let input = HarnessConfigInput::deserialize(deserializer)?;
        let mut config = Self {
            server: input.server,
            agents: input.agents,
            gc: input.gc,
            rules: input.rules,
            observe: input.observe,
            context: input.context,
            otel: input.otel,
            validation: input.validation,
            workspace: input.workspace,
            concurrency: input.concurrency,
            intake: input.intake,
            review: input.review,
            retry_scheduler: input.retry_scheduler,
            reconciliation: input.reconciliation,
            maintenance_window: input.maintenance_window,
            workflow: input.workflow,
            projects: input.projects,
        };
        config.apply_derived_defaults();
        Ok(config)
    }
}

#[cfg(test)]
#[path = "config_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "config_context_tests.rs"]
mod context_tests;

#[cfg(test)]
#[path = "config_workflow_circuit_breaker_tests.rs"]
mod workflow_circuit_breaker_tests;
