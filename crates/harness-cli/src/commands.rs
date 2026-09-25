use clap::{ArgAction, Parser, Subcommand};
use std::path::PathBuf;

mod config;
mod eval;
mod exec;
mod execpolicy;
mod gc;
mod logging;
mod mcp_server;
mod plan;
mod pr;
mod reconcile;
mod rule;
mod runtime;
#[cfg(all(test, feature = "server"))]
mod runtime_log_tests;
#[cfg(feature = "server")]
mod serve;
mod server_cmds;
mod skill;
mod status;
mod version;

#[derive(Parser)]
#[command(name = "harness", about = "Harness — AI Code Agent Platform")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,

    /// Config file path
    #[arg(long, global = true)]
    pub config: Option<PathBuf>,
}

#[derive(Subcommand)]
pub enum Command {
    /// Start the App Server
    Serve {
        /// Transport mode (overrides config file; defaults to config value or "http")
        #[arg(long)]
        transport: Option<String>,
        /// HTTP port (only for http/websocket transport)
        #[arg(long)]
        port: Option<u16>,
        /// Project root used by server-side scans (GC/health)
        #[arg(long)]
        project_root: Option<PathBuf>,
        /// Register a named project at startup (repeatable, format: name=path)
        #[arg(long = "project", value_name = "NAME=PATH")]
        projects: Vec<String>,
        /// Default project name when --project flags are used
        #[arg(long)]
        default_project: Option<String>,
    },

    /// Start MCP Server mode (JSON-RPC over stdio)
    McpServer,

    /// Execute a prompt non-interactively
    Exec {
        /// The prompt to execute
        prompt: String,
        /// Project directory
        #[arg(long)]
        project: Option<PathBuf>,
        /// Agent to use
        #[arg(long, default_value = "auto")]
        agent: String,
        /// Optional model override
        #[arg(long)]
        model: Option<String>,
        /// Sandbox mode hint injected into the exec prompt
        #[arg(long, default_value = "workspace-write")]
        sandbox_mode: String,
        /// Optional output file for final response
        #[arg(long)]
        output_file: Option<PathBuf>,
        /// Refuse execution from sudo/root context by default
        #[arg(long, default_value_t = true, action = ArgAction::Set)]
        drop_sudo: bool,
        /// Require this local OS user for execution
        #[arg(long)]
        unprivileged_user: Option<String>,
        /// Allowed human GitHub actors (comma-separated)
        #[arg(long, value_delimiter = ',')]
        allow_users: Vec<String>,
        /// Allowed bot GitHub actors (comma-separated)
        #[arg(long, value_delimiter = ',')]
        allow_bots: Vec<String>,
        /// GitHub actor identity used with allow lists
        #[arg(long)]
        actor: Option<String>,
    },

    /// Direct GC commands; bypasses server auth, concurrency, and workflow event logging
    Gc {
        #[command(subcommand)]
        cmd: gc::GcCommand,
    },

    /// Direct rule commands; bypasses server auth/concurrency and may record scan events
    Rule {
        #[command(subcommand)]
        cmd: rule::RuleCommand,
    },

    /// Starlark execpolicy commands
    #[command(name = "execpolicy")]
    ExecPolicy {
        #[command(subcommand)]
        cmd: execpolicy::ExecPolicyCommand,
    },

    /// Direct skill commands; bypasses server auth, concurrency, and workflow event logging
    Skill {
        #[command(subcommand)]
        cmd: skill::SkillCommand,
    },

    /// ExecPlan management
    Plan {
        #[command(subcommand)]
        cmd: plan::PlanCommand,
    },

    /// PR orchestration — implement issue and manage PR review loop
    Pr {
        #[command(subcommand)]
        cmd: pr::PrCommand,
    },

    /// Eval run reports and version-to-version diffs
    Eval {
        #[command(subcommand)]
        cmd: eval::EvalCommand,
    },

    /// Display the current version
    Version,

    /// Show live server health, queue, and workflow runtime status
    Status {
        /// Server base URL. Defaults to server.http_addr from config.
        #[arg(long)]
        url: Option<String>,
        /// Filter runtime workflow tree to one project_id.
        #[arg(long)]
        project_id: Option<String>,
        /// Maximum workflow rows requested from the runtime tree endpoint.
        #[arg(long, default_value_t = 20)]
        runtime_limit: i64,
        /// Print raw combined JSON instead of a compact text summary.
        #[arg(long)]
        json: bool,
    },

    /// Workflow runtime operator commands
    Runtime {
        #[command(subcommand)]
        cmd: runtime::RuntimeCommand,
    },

    /// Reconcile harness task state against GitHub PR/issue state
    Reconcile {
        /// Report transitions without applying them
        #[arg(long)]
        dry_run: bool,
        /// Deprecated: reconciliation uses each task's stored project root;
        /// passing this flag returns an error.
        #[arg(long)]
        project: Option<PathBuf>,
    },
}

pub async fn run(cli: Cli) -> anyhow::Result<()> {
    let (mut config, config_source) = config::load_config(cli.config.as_deref())?;
    // Apply env var overrides for all subcommands so that HARNESS_DATA_DIR,
    // HARNESS_PROJECT_ROOT, etc. are respected by gc, rule check, and skill
    // commands — not just `serve`.
    config.apply_env_overrides()?;
    harness_core::db::configure_pg_pool_from_server(&config.server);
    let logging = logging::prepare_logging(&cli.command, &config);
    logging::init_tracing(&logging)?;
    config::log_config_source(&config_source);
    if config_source.capability_profile_defaulted() {
        tracing::warn!(
            effective_capability_profile = "standard",
            permission_mode = "scoped",
            "agents.capability_profile is not configured; the scoped standard default is active; set capability_profile = \"full\" only for an explicit unrestricted opt-up"
        );
    }
    if config.agents.resolve_permission_mode()
        == harness_core::config::agents::AgentPermissionMode::Scoped
        && config.isolation.network_allowlist.is_empty()
    {
        tracing::warn!(network_policy = "deny", "scoped CLI agents have no network access, including model-provider connectivity; configure exact provider hosts in isolation.network_allowlist and use container isolation for allowlisted Linux workloads");
    }
    server_cmds::log_runtime_log_status(&logging);
    config::install_workflow_base(&config_source)?;

    match cli.command {
        Command::Serve {
            transport,
            port,
            project_root,
            projects,
            default_project,
        } => {
            server_cmds::run_serve(
                config,
                transport,
                port,
                project_root,
                projects,
                default_project,
                &logging,
            )
            .await?;
        }

        Command::McpServer => {
            mcp_server::run(config.clone()).await?;
        }

        Command::Exec {
            prompt,
            project,
            agent,
            model,
            sandbox_mode,
            output_file,
            drop_sudo,
            unprivileged_user,
            allow_users,
            allow_bots,
            actor,
        } => {
            exec::run(
                config,
                prompt,
                project,
                agent,
                model,
                sandbox_mode,
                output_file,
                drop_sudo,
                unprivileged_user,
                allow_users,
                allow_bots,
                actor,
            )
            .await?;
        }

        Command::Gc { cmd } => {
            gc::run_gc(cmd, &config).await?;
        }

        Command::Rule { cmd } => {
            rule::run(cmd, &config).await?;
        }

        Command::ExecPolicy { cmd } => {
            execpolicy::run(cmd, &config)?;
        }

        Command::Skill { cmd } => {
            skill::run(cmd, &config)?;
        }

        Command::Pr { cmd } => match cmd {
            pr::PrCommand::Fix { issue, args } => {
                pr::fix(&config, issue, args.wait, args.max_rounds, args.project).await?;
            }
            pr::PrCommand::Loop { pr, args } => {
                pr::loop_pr(&config, pr, args.wait, args.max_rounds, args.project).await?;
            }
            pr::PrCommand::Review { pr, args } => {
                pr::review(&config, pr, args.provider, args.base, args.project).await?;
            }
        },

        Command::Eval { cmd } => {
            eval::run(cmd, &config).await?;
        }

        Command::Plan { cmd } => {
            plan::run(cmd)?;
        }

        Command::Version => {
            version::run()?;
        }

        Command::Status {
            url,
            project_id,
            runtime_limit,
            json,
        } => {
            status::run(&config, url, project_id, runtime_limit, json).await?;
        }

        Command::Runtime { cmd } => {
            runtime::run(&config, cmd).await?;
        }

        Command::Reconcile { dry_run, project } => {
            reconcile::run(dry_run, project, &config).await?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::execpolicy::ExecPolicyCommand;
    use crate::commands::gc::GcCommand;
    use crate::commands::pr::PrCommand;
    use crate::commands::runtime::{RuntimeBreakerCommand, RuntimeCommand};
    use clap::Parser;

    #[test]
    fn cli_parses_serve_with_defaults() {
        let cli =
            Cli::try_parse_from(["harness", "serve"]).expect("serve with defaults should parse");
        match cli.command {
            Command::Serve {
                transport,
                port,
                project_root,
                ..
            } => {
                assert!(transport.is_none());
                assert!(port.is_none());
                assert!(project_root.is_none());
            }
            _ => panic!("expected Serve command"),
        }
    }

    #[test]
    fn cli_parses_serve_with_http_and_port() {
        let cli =
            Cli::try_parse_from(["harness", "serve", "--transport", "http", "--port", "8080"])
                .expect("serve with http+port should parse");
        match cli.command {
            Command::Serve {
                transport, port, ..
            } => {
                assert_eq!(transport.as_deref(), Some("http"));
                assert_eq!(port, Some(8080));
            }
            _ => panic!("expected Serve command"),
        }
    }

    #[test]
    fn cli_parses_status_with_filters() {
        let cli = Cli::try_parse_from([
            "harness",
            "status",
            "--url",
            "127.0.0.1:9800",
            "--project-id",
            "/project-a",
            "--runtime-limit",
            "5",
            "--json",
        ])
        .expect("status with filters should parse");
        match cli.command {
            Command::Status {
                url,
                project_id,
                runtime_limit,
                json,
            } => {
                assert_eq!(url.as_deref(), Some("127.0.0.1:9800"));
                assert_eq!(project_id.as_deref(), Some("/project-a"));
                assert_eq!(runtime_limit, 5);
                assert!(json);
            }
            _ => panic!("expected Status command"),
        }
    }

    #[test]
    fn cli_parses_runtime_breaker_reset() {
        let cli = Cli::try_parse_from([
            "harness",
            "runtime",
            "breaker",
            "reset",
            "codex-default",
            "--url",
            "127.0.0.1:9800",
        ])
        .expect("runtime breaker reset should parse");
        match cli.command {
            Command::Runtime {
                cmd:
                    RuntimeCommand::Breaker {
                        cmd: RuntimeBreakerCommand::Reset { profile, url },
                    },
            } => {
                assert_eq!(profile, "codex-default");
                assert_eq!(url.as_deref(), Some("127.0.0.1:9800"));
            }
            _ => panic!("expected Runtime breaker reset command"),
        }
    }

    #[test]
    fn cli_parses_exec_with_defaults() {
        let cli = Cli::try_parse_from(["harness", "exec", "fix the bug"])
            .expect("exec with defaults should parse");
        match cli.command {
            Command::Exec {
                prompt,
                agent,
                sandbox_mode,
                drop_sudo,
                ..
            } => {
                assert_eq!(prompt, "fix the bug");
                assert_eq!(agent, "auto");
                assert_eq!(sandbox_mode, "workspace-write");
                assert!(drop_sudo);
            }
            _ => panic!("expected Exec command"),
        }
    }

    #[test]
    fn cli_parses_exec_with_all_options() {
        let cli = Cli::try_parse_from([
            "harness",
            "exec",
            "review PR",
            "--project",
            "/tmp/repo",
            "--agent",
            "codex",
            "--model",
            "gpt-4",
            "--sandbox-mode",
            "read-only",
            "--output-file",
            "out.txt",
            "--drop-sudo",
            "false",
            "--unprivileged-user",
            "runner",
            "--allow-users",
            "alice,bob",
            "--allow-bots",
            "dependabot[bot]",
            "--actor",
            "alice",
        ])
        .expect("exec with all options should parse");
        match cli.command {
            Command::Exec {
                prompt,
                agent,
                model,
                sandbox_mode,
                drop_sudo,
                unprivileged_user,
                allow_users,
                allow_bots,
                actor,
                ..
            } => {
                assert_eq!(prompt, "review PR");
                assert_eq!(agent, "codex");
                assert_eq!(model.as_deref(), Some("gpt-4"));
                assert_eq!(sandbox_mode, "read-only");
                assert!(!drop_sudo);
                assert_eq!(unprivileged_user.as_deref(), Some("runner"));
                assert_eq!(allow_users, vec!["alice", "bob"]);
                assert_eq!(allow_bots, vec!["dependabot[bot]"]);
                assert_eq!(actor.as_deref(), Some("alice"));
            }
            _ => panic!("expected Exec command"),
        }
    }

    #[test]
    fn cli_parses_gc_subcommands() {
        let cli = Cli::try_parse_from(["harness", "gc", "run", "/tmp/proj"])
            .expect("gc run should parse");
        match cli.command {
            Command::Gc {
                cmd: GcCommand::Run { project },
            } => {
                assert_eq!(project, Some(PathBuf::from("/tmp/proj")));
            }
            _ => panic!("expected Gc Run command"),
        }

        let cli = Cli::try_parse_from(["harness", "gc", "status"]).expect("gc status should parse");
        assert!(matches!(
            cli.command,
            Command::Gc {
                cmd: GcCommand::Status
            }
        ));

        let cli = Cli::try_parse_from(["harness", "gc", "adopt", "draft-123"])
            .expect("gc adopt should parse");
        match cli.command {
            Command::Gc {
                cmd: GcCommand::Adopt { draft_id },
            } => {
                assert_eq!(draft_id, "draft-123");
            }
            _ => panic!("expected Gc Adopt command"),
        }
    }

    #[test]
    fn cli_parses_pr_fix_subcommand() {
        let cli = Cli::try_parse_from(["harness", "pr", "fix", "42"]).expect("pr fix should parse");
        match cli.command {
            Command::Pr {
                cmd: PrCommand::Fix { issue, args },
            } => {
                assert_eq!(issue, 42);
                assert_eq!(args.wait, 120);
                assert_eq!(args.max_rounds, 8);
            }
            _ => panic!("expected Pr Fix command"),
        }
    }

    #[test]
    fn cli_parses_pr_loop_with_custom_args() {
        let cli = Cli::try_parse_from([
            "harness",
            "pr",
            "loop",
            "99",
            "--wait",
            "30",
            "--max-rounds",
            "3",
        ])
        .expect("pr loop with custom args should parse");
        match cli.command {
            Command::Pr {
                cmd: PrCommand::Loop { pr, args },
            } => {
                assert_eq!(pr, 99);
                assert_eq!(args.wait, 30);
                assert_eq!(args.max_rounds, 3);
            }
            _ => panic!("expected Pr Loop command"),
        }
    }

    #[test]
    fn cli_parses_pr_review_with_provider_and_base() {
        let cli = Cli::try_parse_from([
            "harness",
            "pr",
            "review",
            "99",
            "--provider",
            "codex_cli_review",
            "--base",
            "origin/main",
            "--project",
            "/tmp/project",
        ])
        .expect("pr review should parse");
        match cli.command {
            Command::Pr {
                cmd: PrCommand::Review { pr, args },
            } => {
                assert_eq!(pr, 99);
                assert_eq!(args.provider, "codex_cli_review");
                assert_eq!(args.base.as_deref(), Some("origin/main"));
                assert_eq!(args.project, PathBuf::from("/tmp/project"));
            }
            _ => panic!("expected Pr Review command"),
        }
    }

    #[test]
    fn cli_rejects_exec_without_prompt() {
        let result = Cli::try_parse_from(["harness", "exec"]);
        assert!(result.is_err());
    }

    #[test]
    fn cli_global_config_flag() {
        let cli = Cli::try_parse_from(["harness", "--config", "/etc/harness.toml", "serve"])
            .expect("global config flag should parse");
        assert_eq!(cli.config, Some(PathBuf::from("/etc/harness.toml")));
    }

    #[test]
    fn cli_parses_execpolicy_check_subcommand() {
        let cli = Cli::try_parse_from([
            "harness",
            "execpolicy",
            "check",
            "--rules",
            "policy.star",
            "--pretty",
            "git",
            "status",
        ])
        .expect("execpolicy command should parse");

        match cli.command {
            Command::ExecPolicy { cmd } => match cmd {
                ExecPolicyCommand::Check {
                    rules,
                    requirements,
                    resolve_host_executables,
                    pretty,
                    command,
                } => {
                    assert_eq!(rules, vec![PathBuf::from("policy.star")]);
                    assert_eq!(requirements, None);
                    assert!(!resolve_host_executables);
                    assert!(pretty);
                    assert_eq!(command, vec!["git".to_string(), "status".to_string()]);
                }
            },
            _ => panic!("unexpected command variant parsed"),
        }
    }

    #[test]
    fn cli_parses_serve_with_single_project_flag() {
        let cli =
            Cli::try_parse_from(["harness", "serve", "--project", "harness=/path/to/harness"])
                .expect("serve with --project should parse");
        match cli.command {
            Command::Serve { projects, .. } => {
                assert_eq!(projects, vec!["harness=/path/to/harness"]);
            }
            _ => panic!("expected Serve command"),
        }
    }

    #[test]
    fn cli_parses_serve_with_multiple_project_flags() {
        let cli = Cli::try_parse_from([
            "harness",
            "serve",
            "--project",
            "harness=/path/to/harness",
            "--project",
            "litellm=/path/to/litellm-rs",
        ])
        .expect("serve with multiple --project flags should parse");
        match cli.command {
            Command::Serve { projects, .. } => {
                assert_eq!(
                    projects,
                    vec!["harness=/path/to/harness", "litellm=/path/to/litellm-rs"]
                );
            }
            _ => panic!("expected Serve command"),
        }
    }

    #[test]
    fn cli_parses_serve_with_default_project_flag() {
        let cli = Cli::try_parse_from([
            "harness",
            "serve",
            "--project",
            "harness=/path/to/harness",
            "--project",
            "litellm=/path/to/litellm-rs",
            "--default-project",
            "litellm",
        ])
        .expect("serve with --default-project should parse");
        match cli.command {
            Command::Serve {
                projects,
                default_project,
                ..
            } => {
                assert_eq!(projects.len(), 2);
                assert_eq!(default_project.as_deref(), Some("litellm"));
            }
            _ => panic!("expected Serve command"),
        }
    }

    #[test]
    fn cli_parses_serve_project_root_still_works() {
        let cli = Cli::try_parse_from(["harness", "serve", "--project-root", "/tmp/repo"])
            .expect("--project-root backward compat should parse");
        match cli.command {
            Command::Serve { project_root, .. } => {
                assert_eq!(project_root, Some(PathBuf::from("/tmp/repo")));
            }
            _ => panic!("expected Serve command"),
        }
    }
}
