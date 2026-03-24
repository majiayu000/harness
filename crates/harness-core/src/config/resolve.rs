use super::project::{ProjectConfig, ReviewType};
use super::{AgentReviewConfig, ConcurrencyConfig, GcConfig, HarnessConfig};

/// Effective configuration for a task, merging server defaults with per-project overrides.
///
/// Produced by [`resolve_config`]. Project values take precedence over server
/// defaults when present (`Some`). `None` fields fall through to the server default.
#[derive(Debug, Clone)]
pub struct ResolvedConfig {
    /// Effective default agent name.
    pub default_agent: String,
    /// Effective agent review configuration.
    pub review: AgentReviewConfig,
    /// Effective concurrency configuration.
    pub concurrency: ConcurrencyConfig,
    /// Effective GC configuration.
    pub gc: GcConfig,
    /// Project type for review focus selection.
    pub review_type: ReviewType,
    /// Per-project override: seconds to wait for review bot before the review loop.
    pub review_wait_secs: Option<u64>,
    /// Per-project override: maximum review bot rounds.
    pub review_max_rounds: Option<u32>,
}

/// Merge server-level defaults with per-project overrides.
///
/// Fields present in `project` (i.e. `Some`) override the corresponding
/// server defaults. Missing fields (`None`) inherit the server value unchanged.
pub fn resolve_config(server: &HarnessConfig, project: &ProjectConfig) -> ResolvedConfig {
    let default_agent = project
        .agent
        .as_ref()
        .and_then(|a| a.default.clone())
        .unwrap_or_else(|| server.agents.default_agent.clone());

    let mut review = server.agents.review.clone();
    if let Some(proj_review) = &project.review {
        if let Some(enabled) = proj_review.enabled {
            review.enabled = enabled;
        }
        if let Some(cmd) = &proj_review.bot_command {
            review.review_bot_command = cmd.clone();
        }
    }

    let mut concurrency = server.concurrency.clone();
    if let Some(proj_concurrency) = &project.concurrency {
        if let Some(max) = proj_concurrency.max_concurrent_tasks {
            concurrency.max_concurrent_tasks = max;
        }
    }

    let mut gc = server.gc.clone();
    if let Some(proj_gc) = &project.gc {
        if let Some(max_drafts) = proj_gc.max_drafts_per_run {
            gc.max_drafts_per_run = max_drafts;
        }
    }

    let mut review_wait_secs: Option<u64> = None;
    let mut review_max_rounds: Option<u32> = None;
    if let Some(proj_review) = &project.review {
        if let Some(secs) = proj_review.review_wait_secs {
            review_wait_secs = Some(secs);
        }
        if let Some(rounds) = proj_review.review_max_rounds {
            review_max_rounds = Some(rounds);
        }
    }

    ResolvedConfig {
        default_agent,
        review,
        concurrency,
        gc,
        review_type: project.review_type,
        review_wait_secs,
        review_max_rounds,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::project::{
        ProjectAgentConfig, ProjectConcurrencyConfig, ProjectGcConfig, ProjectReviewConfig,
    };

    #[test]
    fn resolve_config_inherits_all_server_defaults_when_project_is_empty() {
        let server = HarnessConfig::default();
        let project = ProjectConfig::default();
        let resolved = resolve_config(&server, &project);

        assert_eq!(resolved.default_agent, server.agents.default_agent);
        assert_eq!(resolved.review.enabled, server.agents.review.enabled);
        assert_eq!(
            resolved.review.review_bot_command,
            server.agents.review.review_bot_command
        );
        assert_eq!(
            resolved.concurrency.max_concurrent_tasks,
            server.concurrency.max_concurrent_tasks
        );
        assert_eq!(resolved.gc.max_drafts_per_run, server.gc.max_drafts_per_run);
    }

    #[test]
    fn resolve_config_project_agent_overrides_default_agent() {
        let server = HarnessConfig::default();
        let project = ProjectConfig {
            agent: Some(ProjectAgentConfig {
                default: Some("codex".to_string()),
            }),
            ..Default::default()
        };

        let resolved = resolve_config(&server, &project);
        assert_eq!(resolved.default_agent, "codex");
    }

    #[test]
    fn resolve_config_partial_review_override_only_changes_specified_fields() {
        let server = HarnessConfig::default();
        let project = ProjectConfig {
            review: Some(ProjectReviewConfig {
                enabled: Some(true),
                bot_command: None,
                review_wait_secs: None,
                review_max_rounds: None,
            }),
            ..Default::default()
        };

        let resolved = resolve_config(&server, &project);
        assert!(resolved.review.enabled);
        // bot_command falls back to server default
        assert_eq!(
            resolved.review.review_bot_command,
            server.agents.review.review_bot_command
        );
    }

    #[test]
    fn resolve_config_review_bot_command_overrides_server_default() {
        let server = HarnessConfig::default();
        let project = ProjectConfig {
            review: Some(ProjectReviewConfig {
                enabled: Some(true),
                bot_command: Some("/custom review".to_string()),
                review_wait_secs: None,
                review_max_rounds: None,
            }),
            ..Default::default()
        };

        let resolved = resolve_config(&server, &project);
        assert!(resolved.review.enabled);
        assert_eq!(resolved.review.review_bot_command, "/custom review");
    }

    #[test]
    fn resolve_config_concurrency_override() {
        let server = HarnessConfig::default();
        let project = ProjectConfig {
            concurrency: Some(ProjectConcurrencyConfig {
                max_concurrent_tasks: Some(3),
            }),
            ..Default::default()
        };

        let resolved = resolve_config(&server, &project);
        assert_eq!(resolved.concurrency.max_concurrent_tasks, 3);
    }

    #[test]
    fn resolve_config_gc_override() {
        let server = HarnessConfig::default();
        let project = ProjectConfig {
            gc: Some(ProjectGcConfig {
                max_drafts_per_run: Some(10),
            }),
            ..Default::default()
        };

        let resolved = resolve_config(&server, &project);
        assert_eq!(resolved.gc.max_drafts_per_run, 10);
    }

    #[test]
    fn resolve_config_mixed_partial_overrides() {
        let server = HarnessConfig::default();
        let project = ProjectConfig {
            agent: Some(ProjectAgentConfig {
                default: Some("codex".to_string()),
            }),
            concurrency: Some(ProjectConcurrencyConfig {
                max_concurrent_tasks: Some(2),
            }),
            ..Default::default()
        };
        // review and gc inherit server defaults

        let resolved = resolve_config(&server, &project);
        assert_eq!(resolved.default_agent, "codex");
        assert_eq!(resolved.concurrency.max_concurrent_tasks, 2);
        assert_eq!(resolved.review.enabled, server.agents.review.enabled);
        assert_eq!(resolved.gc.max_drafts_per_run, server.gc.max_drafts_per_run);
    }
}
