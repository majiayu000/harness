pub mod agents;
pub mod dirs;
pub mod intake;
pub mod misc;
pub mod project;
pub mod resolve;
pub mod server;

use self::agents::*;
use self::intake::*;
use self::misc::*;
use self::server::*;
use serde::{Deserialize, Serialize};
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

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HarnessConfig {
    pub server: ServerConfig,
    pub agents: AgentsConfig,
    pub gc: GcConfig,
    pub rules: RulesConfig,
    pub observe: ObserveConfig,
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
        self.server.apply_env_overrides()?;
        Ok(())
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
        rebase(&mut self.workspace.root, base);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_review_config_defaults() {
        let config = AgentReviewConfig::default();
        assert!(!config.enabled);
        assert!(config.reviewer_agent.is_empty());
        assert_eq!(config.max_rounds, 3);
        assert_eq!(config.review_bot_command, "/gemini review");
        assert!(config.review_bot_auto_trigger);
    }

    #[test]
    fn agent_review_config_deserializes_from_toml() {
        let toml_str = r#"
            enabled = true
            reviewer_agent = "codex"
            max_rounds = 5
        "#;
        let config: AgentReviewConfig = toml::from_str(toml_str).unwrap();
        assert!(config.enabled);
        assert_eq!(config.reviewer_agent, "codex");
        assert_eq!(config.max_rounds, 5);
        assert_eq!(config.review_bot_command, "/gemini review");
        assert!(config.review_bot_auto_trigger);
    }

    #[test]
    fn agent_review_config_deserializes_with_defaults() {
        let toml_str = r#"
            enabled = true
        "#;
        let config: AgentReviewConfig = toml::from_str(toml_str).unwrap();
        assert!(config.enabled);
        assert!(config.reviewer_agent.is_empty());
        assert_eq!(config.max_rounds, 3);
        assert_eq!(config.review_bot_command, "/gemini review");
        assert!(config.review_bot_auto_trigger);
    }

    #[test]
    fn agent_review_config_deserializes_bot_settings() {
        let toml_str = r#"
            enabled = true
            review_bot_command = "/reviewbot run"
            review_bot_auto_trigger = false
        "#;
        let config: AgentReviewConfig = toml::from_str(toml_str).unwrap();
        assert!(config.enabled);
        assert_eq!(config.review_bot_command, "/reviewbot run");
        assert!(!config.review_bot_auto_trigger);
    }

    #[test]
    fn agents_config_includes_review() {
        let toml_str = r#"
            default_agent = "claude"
            [claude]
            cli_path = "claude"
            default_model = "sonnet"
            [codex]
            cli_path = "codex"
            [anthropic_api]
            base_url = "https://api.anthropic.com"
            default_model = "claude-sonnet-4-6"
            [review]
            enabled = true
            reviewer_agent = "codex"
            max_rounds = 2
        "#;
        let config: AgentsConfig = toml::from_str(toml_str).unwrap();
        assert!(config.review.enabled);
        assert_eq!(config.review.reviewer_agent, "codex");
        assert_eq!(config.review.max_rounds, 2);
        assert_eq!(config.sandbox_mode, SandboxMode::DangerFullAccess);
    }

    #[test]
    fn anthropic_api_config_deserializes_configured_max_tokens() {
        let toml_str = r#"
            base_url = "https://api.anthropic.com"
            default_model = "claude-sonnet-4-6"
            max_tokens = 8192
        "#;
        let config: AnthropicApiConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.max_tokens, 8192);
    }

    #[test]
    fn approval_policy_defaults_to_auto_edit() {
        let config = AgentsConfig::default();
        assert_eq!(config.approval_policy, ApprovalPolicy::AutoEdit);
    }

    #[test]
    fn sandbox_mode_defaults_to_danger_full_access() {
        let config = AgentsConfig::default();
        assert_eq!(config.sandbox_mode, SandboxMode::DangerFullAccess);
    }

    #[test]
    fn approval_policy_deserializes_from_toml() {
        let toml_str = r#"
            default_agent = "claude"
            approval_policy = "full_auto"
            [claude]
            cli_path = "claude"
            default_model = "sonnet"
            [codex]
            cli_path = "codex"
            [anthropic_api]
            base_url = "https://api.anthropic.com"
            default_model = "claude-sonnet-4-6"
        "#;
        let config: AgentsConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.approval_policy, ApprovalPolicy::FullAuto);
    }

    #[test]
    fn sandbox_mode_deserializes_from_toml() {
        let toml_str = r#"
            default_agent = "claude"
            sandbox_mode = "danger-full-access"
            [claude]
            cli_path = "claude"
            default_model = "sonnet"
            [codex]
            cli_path = "codex"
            [anthropic_api]
            base_url = "https://api.anthropic.com"
            default_model = "claude-sonnet-4-6"
        "#;
        let config: AgentsConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.sandbox_mode, SandboxMode::DangerFullAccess);
    }

    #[test]
    fn anthropic_api_config_defaults_max_tokens_when_missing() {
        let toml_str = r#"
            base_url = "https://api.anthropic.com"
            default_model = "claude-sonnet-4-6"
        "#;
        let config: AnthropicApiConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.max_tokens, 4096);
    }

    #[test]
    fn codex_cloud_config_defaults() {
        let config = CodexCloudConfig::default();
        assert!(!config.enabled);
        assert_eq!(config.cache_ttl_hours, 12);
        assert!(config.setup_commands.is_empty());
        assert!(config.setup_secret_env.is_empty());
    }

    #[test]
    fn codex_agent_config_deserializes_cloud_block() {
        let toml_str = r#"
            cli_path = "codex"
            [cloud]
            enabled = true
            cache_ttl_hours = 6
            setup_commands = ["npm ci", "cargo fetch"]
            setup_secret_env = ["NPM_TOKEN"]
        "#;
        let config: CodexAgentConfig = toml::from_str(toml_str).unwrap();
        assert!(config.cloud.enabled);
        assert_eq!(config.cloud.cache_ttl_hours, 6);
        assert_eq!(
            config.cloud.setup_commands,
            vec!["npm ci".to_string(), "cargo fetch".to_string()]
        );
        assert_eq!(config.cloud.setup_secret_env, vec!["NPM_TOKEN".to_string()]);
    }

    #[test]
    fn rules_config_defaults_do_not_autoload_requirements() {
        let rules = RulesConfig::default();
        assert!(rules.exec_policy_paths.is_empty());
        assert_eq!(rules.requirements_path, None);
    }

    #[test]
    fn rules_config_deserializes_when_new_fields_are_missing() {
        let toml_str = r#"
            discovery_paths = []
        "#;
        let rules: RulesConfig = toml::from_str(toml_str).unwrap();
        assert!(rules.exec_policy_paths.is_empty());
        assert_eq!(rules.requirements_path, None);
    }

    #[test]
    fn otel_config_defaults_to_disabled_exporter() {
        let config = OtelConfig::default();
        assert_eq!(config.exporter, OtelExporter::Disabled);
        assert!(config.endpoint.is_none());
        assert!(!config.log_user_prompt);
        assert_eq!(config.environment, "development");
    }

    #[test]
    fn harness_config_deserializes_otel_section() {
        let toml_str = r#"
            [server]
            transport = "stdio"
            http_addr = "127.0.0.1:9800"
            data_dir = "."
            project_root = "."

            [agents]
            default_agent = "claude"
            [agents.claude]
            cli_path = "claude"
            default_model = "sonnet"
            [agents.codex]
            cli_path = "codex"
            [agents.anthropic_api]
            base_url = "https://api.anthropic.com"
            default_model = "claude-sonnet-4-6"

            [gc]
            max_drafts_per_run = 5
            budget_per_signal_usd = 0.5
            total_budget_usd = 5.0
            [gc.signal_thresholds]
            repeated_warn_min = 10
            chronic_block_min = 5
            hot_file_edits_min = 20
            slow_op_threshold_ms = 5000
            slow_op_count_min = 10
            escalation_ratio = 1.5
            violation_min = 5

            [rules]
            discovery_paths = []

            [observe]
            session_renewal_secs = 1800
            log_retention_days = 90

            [otel]
            environment = "staging"
            exporter = "otlp-http"
            endpoint = "http://collector:4318"
            log_user_prompt = true
        "#;

        let config: HarnessConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.otel.environment, "staging");
        assert_eq!(config.otel.exporter, OtelExporter::OtlpHttp);
        assert_eq!(
            config.otel.endpoint.as_deref(),
            Some("http://collector:4318")
        );
        assert!(config.otel.log_user_prompt);
    }

    #[test]
    fn server_config_webhook_secret_defaults_to_none() {
        let config = ServerConfig::default();
        assert!(config.github_webhook_secret.is_none());
    }

    #[test]
    fn server_config_deserializes_webhook_secret() {
        let toml_str = r#"
            transport = "http"
            http_addr = "127.0.0.1:9800"
            data_dir = "/tmp/harness"
            project_root = "."
            github_webhook_secret = "super-secret"
        "#;
        let config: ServerConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(
            config.github_webhook_secret.as_deref(),
            Some("super-secret")
        );
    }

    #[derive(Deserialize)]
    struct TransportWrapper {
        t: Transport,
    }

    #[test]
    fn transport_deserializes_all_variants() {
        let parse = |s: &str| -> Transport {
            toml::from_str::<TransportWrapper>(&format!("t = \"{s}\""))
                .unwrap_or_else(|e| panic!("failed to parse transport `{s}`: {e}"))
                .t
        };
        assert_eq!(parse("stdio"), Transport::Stdio);
        assert_eq!(parse("http"), Transport::Http);
        assert_eq!(parse("web_socket"), Transport::WebSocket);
    }

    #[test]
    fn transport_rejects_unknown_variant() {
        let result = toml::from_str::<TransportWrapper>(r#"t = "grpc""#);
        assert!(result.is_err());
    }

    #[test]
    fn gc_config_defaults_are_consistent() {
        let config = GcConfig::default();
        assert_eq!(config.max_drafts_per_run, 5);
        assert_eq!(config.adopt_wait_secs, 120);
        assert_eq!(config.adopt_max_rounds, 3);
        assert_eq!(config.adopt_turn_timeout_secs, 600);
        assert!(config.budget_per_signal_usd > 0.0);
        assert!(config.total_budget_usd >= config.budget_per_signal_usd);
    }

    #[test]
    fn gc_config_deserializes_from_toml() {
        let toml_str = r#"
            max_drafts_per_run = 10
            budget_per_signal_usd = 1.0
            total_budget_usd = 20.0
            adopt_wait_secs = 60
            adopt_max_rounds = 5
            adopt_turn_timeout_secs = 300

            [signal_thresholds]
            repeated_warn_min = 20
            chronic_block_min = 10
            hot_file_edits_min = 30
            slow_op_threshold_ms = 3000
            slow_op_count_min = 5
            escalation_ratio = 2.0
            violation_min = 3
        "#;
        let config: GcConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.max_drafts_per_run, 10);
        assert_eq!(config.adopt_wait_secs, 60);
        assert_eq!(config.signal_thresholds.repeated_warn_min, 20);
        assert_eq!(config.signal_thresholds.escalation_ratio, 2.0);
    }

    #[test]
    fn signal_thresholds_defaults_are_consistent() {
        let thresholds = SignalThresholdsConfig::default();
        assert_eq!(thresholds.repeated_warn_min, 10);
        assert_eq!(thresholds.chronic_block_min, 5);
        assert_eq!(thresholds.hot_file_edits_min, 20);
        assert_eq!(thresholds.slow_op_threshold_ms, 5000);
        assert_eq!(thresholds.slow_op_count_min, 10);
        assert_eq!(thresholds.violation_min, 5);
        assert!(thresholds.escalation_ratio > 1.0);
    }

    #[test]
    fn harness_config_default_roundtrip() {
        let config = HarnessConfig::default();
        assert_eq!(config.agents.default_agent, "auto");
        assert!(config.agents.complexity_preferred_agents.is_empty());
        assert_eq!(config.server.transport, Transport::Http);
        assert!(!config.agents.review.enabled);
        assert_eq!(config.otel.exporter, OtelExporter::Disabled);
        assert_eq!(config.gc.max_drafts_per_run, 5);
        assert!(!config.review.enabled);
        assert!(!config.review.run_on_startup);
        assert_eq!(config.review.interval_hours, 24);
        assert!(config.review.interval_secs.is_none());
        assert_eq!(config.review.timeout_secs, 900);
        assert!(config.review.agent.is_none());
    }

    #[test]
    fn review_config_defaults() {
        let config = ReviewConfig::default();
        assert!(!config.enabled);
        assert!(!config.run_on_startup);
        assert_eq!(config.interval_hours, 24);
        assert!(config.interval_secs.is_none());
        assert_eq!(config.timeout_secs, 900);
        assert!(config.agent.is_none());
    }

    #[test]
    fn review_config_deserializes_from_toml() {
        let toml_str = r#"
            enabled = true
            run_on_startup = true
            interval_hours = 48
            interval_secs = 600
            agent = "claude"
            timeout_secs = 1200
        "#;
        let config: ReviewConfig = toml::from_str(toml_str).unwrap();
        assert!(config.enabled);
        assert!(config.run_on_startup);
        assert_eq!(config.interval_hours, 48);
        assert_eq!(config.interval_secs, Some(600));
        assert_eq!(config.agent.as_deref(), Some("claude"));
        assert_eq!(config.timeout_secs, 1200);
    }

    #[test]
    fn review_config_deserializes_with_defaults() {
        let config: ReviewConfig = toml::from_str("enabled = true").unwrap();
        assert!(config.enabled);
        assert!(!config.run_on_startup);
        assert_eq!(config.interval_hours, 24);
        assert!(config.interval_secs.is_none());
        assert!(config.agent.is_none());
        assert_eq!(config.timeout_secs, 900);
    }

    #[test]
    fn harness_config_includes_review() {
        let config = HarnessConfig::default();
        assert!(!config.review.enabled);
    }

    #[test]
    fn projects_config_deserializes_from_toml() {
        let toml_str = r#"
            [[projects]]
            name = "harness"
            root = "/tmp/harness"
            default = true

            [[projects]]
            name = "litellm"
            root = "/tmp/litellm-rs"
            max_concurrent = 2
            default_agent = "claude"
        "#;
        #[derive(Deserialize)]
        struct Wrapper {
            #[serde(default)]
            projects: Vec<ProjectEntry>,
        }
        let w: Wrapper = toml::from_str(toml_str).unwrap();
        assert_eq!(w.projects.len(), 2);
        assert_eq!(w.projects[0].name, "harness");
        assert!(w.projects[0].default);
        assert!(w.projects[0].max_concurrent.is_none());
        assert_eq!(w.projects[1].name, "litellm");
        assert!(!w.projects[1].default);
        assert_eq!(w.projects[1].max_concurrent, Some(2));
        assert_eq!(w.projects[1].default_agent.as_deref(), Some("claude"));
    }

    #[test]
    fn harness_config_projects_defaults_to_empty() {
        let config = HarnessConfig::default();
        assert!(config.projects.is_empty());
    }

    #[test]
    fn invalid_toml_returns_parse_error() {
        let result = toml::from_str::<HarnessConfig>("not valid toml {{{}}}");
        assert!(result.is_err());
    }

    #[test]
    fn rebase_relative_paths_resolves_against_base() {
        let mut config = HarnessConfig::default();
        config.server.data_dir = PathBuf::from("data");
        config.server.project_root = PathBuf::from("repo");
        config.server.allowed_project_roots = vec![PathBuf::from("allowed")];
        config.projects = vec![ProjectEntry {
            name: "p".into(),
            root: PathBuf::from("projects/p"),
            default: false,
            default_agent: None,
            max_concurrent: None,
        }];

        config.rebase_relative_paths(std::path::Path::new("/etc/harness"));

        assert_eq!(config.server.data_dir, PathBuf::from("/etc/harness/data"));
        assert_eq!(
            config.server.project_root,
            PathBuf::from("/etc/harness/repo")
        );
        assert_eq!(
            config.server.allowed_project_roots,
            vec![PathBuf::from("/etc/harness/allowed")]
        );
        assert_eq!(
            config.projects[0].root,
            PathBuf::from("/etc/harness/projects/p")
        );
    }

    #[test]
    fn rebase_relative_paths_leaves_absolute_paths_unchanged() {
        let mut config = HarnessConfig::default();
        config.server.data_dir = PathBuf::from("/absolute/data");
        config.server.project_root = PathBuf::from("/absolute/repo");

        config.rebase_relative_paths(std::path::Path::new("/etc/harness"));

        assert_eq!(config.server.data_dir, PathBuf::from("/absolute/data"));
        assert_eq!(config.server.project_root, PathBuf::from("/absolute/repo"));
    }

    #[test]
    fn github_intake_config_defaults() {
        let config = GitHubIntakeConfig::default();
        assert!(!config.enabled);
        assert!(config.repo.is_empty());
        assert_eq!(config.label, "harness");
        assert_eq!(config.poll_interval_secs, 30);
    }

    #[test]
    fn github_intake_config_deserializes_from_toml() {
        let toml_str = r#"
            enabled = true
            repo = "owner/repo"
            label = "autofix"
            poll_interval_secs = 60
        "#;
        let config: GitHubIntakeConfig = toml::from_str(toml_str).unwrap();
        assert!(config.enabled);
        assert_eq!(config.repo, "owner/repo");
        assert_eq!(config.label, "autofix");
        assert_eq!(config.poll_interval_secs, 60);
    }

    #[test]
    fn intake_config_defaults_to_no_github() {
        let config = IntakeConfig::default();
        assert!(config.github.is_none());
    }

    #[test]
    fn harness_config_includes_intake() {
        let config = HarnessConfig::default();
        assert!(config.intake.github.is_none());
    }

    #[test]
    fn workspace_config_defaults() {
        let config = WorkspaceConfig::default();
        assert!(config.after_create_hook.is_none());
        assert!(config.before_remove_hook.is_none());
        assert_eq!(config.hook_timeout_secs, 60);
        assert!(config.auto_cleanup);
    }

    #[test]
    fn workspace_config_deserializes_from_toml() {
        let toml_str = r#"
            after_create_hook = "npm install"
            before_remove_hook = "echo cleanup"
            hook_timeout_secs = 30
            auto_cleanup = false
        "#;
        let config: WorkspaceConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.after_create_hook.as_deref(), Some("npm install"));
        assert_eq!(config.before_remove_hook.as_deref(), Some("echo cleanup"));
        assert_eq!(config.hook_timeout_secs, 30);
        assert!(!config.auto_cleanup);
    }

    #[test]
    fn harness_config_includes_workspace() {
        let config = HarnessConfig::default();
        assert!(config.workspace.auto_cleanup);
        assert_eq!(config.workspace.hook_timeout_secs, 60);
    }

    #[test]
    fn otel_exporter_grpc_variant_deserializes() {
        let toml_str = r#"
            environment = "production"
            exporter = "otlp-grpc"
            endpoint = "http://otel-collector:4317"
        "#;
        let config: OtelConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.exporter, OtelExporter::OtlpGrpc);
        assert_eq!(
            config.endpoint.as_deref(),
            Some("http://otel-collector:4317")
        );
    }

    #[test]
    fn agent_review_config_review_bot_command_default() {
        let config = AgentReviewConfig::default();
        assert_eq!(config.review_bot_command, "/gemini review");
    }

    #[test]
    fn server_config_notification_defaults() {
        let config = ServerConfig::default();
        assert_eq!(config.notification_broadcast_capacity, 256);
        assert_eq!(config.notification_lag_log_every, 1);
    }

    #[test]
    fn validation_config_defaults() {
        let config = ValidationConfig::default();
        assert!(config.pre_commit.is_empty());
        assert!(config.pre_push.is_empty());
        assert_eq!(config.timeout_secs, 120);
        assert_eq!(config.max_retries, 2);
    }

    #[test]
    fn validation_config_deserializes_from_toml() {
        let toml_str = r#"
            pre_commit = ["cargo fmt --all -- --check", "cargo check"]
            pre_push = ["cargo test"]
            timeout_secs = 60
            max_retries = 3
        "#;
        let config: ValidationConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.pre_commit.len(), 2);
        assert_eq!(config.pre_push, vec!["cargo test"]);
        assert_eq!(config.timeout_secs, 60);
        assert_eq!(config.max_retries, 3);
    }

    #[test]
    fn validation_config_deserializes_with_defaults() {
        let toml_str = r#"
            pre_commit = ["ruff check ."]
        "#;
        let config: ValidationConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.pre_commit, vec!["ruff check ."]);
        assert!(config.pre_push.is_empty());
        assert_eq!(config.timeout_secs, 120);
        assert_eq!(config.max_retries, 2);
    }
}
