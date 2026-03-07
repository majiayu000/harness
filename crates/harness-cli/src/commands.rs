use anyhow::Context;
use clap::{Args, Parser, Subcommand};
use std::path::PathBuf;
use std::sync::Arc;

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
        /// Transport mode
        #[arg(long, default_value = "stdio")]
        transport: String,
        /// HTTP port (only for http/websocket transport)
        #[arg(long)]
        port: Option<u16>,
        /// Project root used by server-side scans (GC/health)
        #[arg(long)]
        project_root: Option<PathBuf>,
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
        #[arg(long, default_value = "claude")]
        agent: String,
    },

    /// GC Agent commands
    Gc {
        #[command(subcommand)]
        cmd: GcCommand,
    },

    /// Rule engine commands
    Rule {
        #[command(subcommand)]
        cmd: RuleCommand,
    },

    /// Starlark execpolicy commands
    #[command(name = "execpolicy")]
    ExecPolicy {
        #[command(subcommand)]
        cmd: ExecPolicyCommand,
    },

    /// Skill system commands
    Skill {
        #[command(subcommand)]
        cmd: SkillCommand,
    },

    /// ExecPlan management
    Plan {
        #[command(subcommand)]
        cmd: PlanCommand,
    },

    /// PR orchestration — implement issue and manage PR review loop
    Pr {
        #[command(subcommand)]
        cmd: PrCommand,
    },
}

#[derive(Subcommand)]
pub enum GcCommand {
    /// Run GC agent
    Run {
        /// Project directory
        project: Option<PathBuf>,
    },
    /// Show GC status
    Status,
    /// List pending drafts
    Drafts { project: Option<PathBuf> },
    /// Adopt a draft
    Adopt { draft_id: String },
    /// Reject a draft
    Reject {
        draft_id: String,
        #[arg(long)]
        reason: Option<String>,
    },
}

#[derive(Subcommand)]
pub enum RuleCommand {
    /// Load rules for a project
    Load {
        /// Project directory
        #[arg(default_value = ".")]
        project: PathBuf,
    },
    /// Check project for violations
    Check {
        /// Project directory
        #[arg(default_value = ".")]
        project: PathBuf,
    },
}

#[derive(Subcommand)]
pub enum ExecPolicyCommand {
    /// Check a command against Starlark policy rules
    Check {
        /// Paths to policy files (repeatable). Falls back to `rules.exec_policy_paths`.
        #[arg(short = 'r', long = "rules", value_name = "PATH")]
        rules: Vec<PathBuf>,
        /// Optional requirements.toml path. Falls back to `rules.requirements_path` when omitted.
        #[arg(long, value_name = "PATH")]
        requirements: Option<PathBuf>,
        /// Resolve absolute executables against basename rules.
        #[arg(long)]
        resolve_host_executables: bool,
        /// Pretty-print JSON output.
        #[arg(long)]
        pretty: bool,
        /// Command tokens to evaluate.
        #[arg(
            value_name = "COMMAND",
            required = true,
            trailing_var_arg = true,
            allow_hyphen_values = true
        )]
        command: Vec<String>,
    },
}

#[derive(Subcommand)]
pub enum SkillCommand {
    /// List available skills
    List {
        #[arg(long)]
        query: Option<String>,
    },
    /// Create a new skill
    Create {
        name: String,
        #[arg(long)]
        file: PathBuf,
    },
    /// Delete a skill
    Delete { skill_id: String },
}

#[derive(Args)]
pub struct LoopArgs {
    /// Seconds to wait between review rounds (for CI and review bots)
    #[arg(long, default_value = "120")]
    pub wait: u64,
    /// Maximum number of review rounds
    #[arg(long, default_value = "5")]
    pub max_rounds: u32,
    /// Project directory
    #[arg(long, default_value = ".")]
    pub project: std::path::PathBuf,
}

#[derive(Subcommand)]
pub enum PrCommand {
    /// Implement a GitHub issue, create a PR, then run the review loop
    Fix {
        /// GitHub issue number
        issue: u64,
        #[command(flatten)]
        args: LoopArgs,
    },
    /// Run the review loop for an existing PR
    Loop {
        /// GitHub PR number
        pr: u64,
        #[command(flatten)]
        args: LoopArgs,
    },
}

#[derive(Subcommand)]
pub enum PlanCommand {
    /// Initialize a new ExecPlan from a spec
    Init {
        /// Path to spec file
        spec: PathBuf,
    },
    /// Show ExecPlan status
    Status {
        /// Plan ID or file path
        plan: String,
    },
}

fn configured_rule_engine(config: &harness_core::HarnessConfig) -> harness_rules::RuleEngine {
    let mut engine = harness_rules::RuleEngine::new();
    engine.configure_sources(
        config.rules.discovery_paths.clone(),
        config.rules.builtin_path.clone(),
        config.rules.requirements_path.clone(),
    );
    engine
}

fn resolve_exec_project_root(project: Option<PathBuf>) -> anyhow::Result<PathBuf> {
    resolve_exec_project_root_with(project, std::env::current_dir)
}

fn resolve_exec_project_root_with<F>(
    project: Option<PathBuf>,
    current_dir: F,
) -> anyhow::Result<PathBuf>
where
    F: FnOnce() -> std::io::Result<PathBuf>,
{
    if let Some(project_root) = project {
        return Ok(project_root);
    }

    let project_root = current_dir().with_context(|| {
        "failed to resolve `harness exec` project root from current working directory"
    });

    if let Err(error) = &project_root {
        tracing::error!(
            error = %error,
            "unable to determine project root for `harness exec`; pass --project to override"
        );
    }

    project_root
}

pub async fn run(cli: Cli) -> anyhow::Result<()> {
    // Load config
    let config = if let Some(config_path) = &cli.config {
        let content = std::fs::read_to_string(config_path)?;
        toml::from_str(&content)?
    } else {
        harness_core::HarnessConfig::default()
    };

    match cli.command {
        Command::Serve {
            transport,
            port,
            project_root,
        } => {
            let mut serve_config = config.clone();
            if let Some(project_root) = project_root {
                serve_config.server.project_root = project_root;
            }
            let thread_manager = harness_server::thread_manager::ThreadManager::new();
            let mut agent_registry =
                harness_agents::AgentRegistry::new(&serve_config.agents.default_agent);
            agent_registry.register(
                "claude",
                Arc::new(harness_agents::claude::ClaudeCodeAgent::new(
                    serve_config.agents.claude.cli_path.clone(),
                    serve_config.agents.claude.default_model.clone(),
                )),
            );
            agent_registry.register(
                "codex",
                Arc::new(harness_agents::codex::CodexAgent::new(
                    serve_config.agents.codex.cli_path.clone(),
                )),
            );
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
        }

        Command::McpServer => {
            crate::cmd::mcp_server::run(config.clone()).await?;
        }

        Command::Exec {
            prompt,
            project,
            agent: _agent_name,
        } => {
            let project_root = resolve_exec_project_root(project)?;
            let agent = harness_agents::claude::ClaudeCodeAgent::new(
                config.agents.claude.cli_path.clone(),
                config.agents.claude.default_model.clone(),
            );

            let req = harness_core::AgentRequest {
                prompt,
                project_root,
                ..Default::default()
            };

            let resp = harness_core::CodeAgent::execute(&agent, req).await?;
            println!("{}", resp.output);
            if !resp.stderr.is_empty() {
                eprintln!("[harness] agent stderr:\n{}", resp.stderr);
            }
        }

        Command::Gc { cmd } => {
            crate::gc::run_gc(cmd, &config).await?;
        }

        Command::Rule { cmd } => {
            match cmd {
                RuleCommand::Load { project } => {
                    let mut engine = configured_rule_engine(&config);
                    engine.load(&project)?;
                    println!("Loaded {} rules", engine.rules().len());
                }
                RuleCommand::Check { project } => {
                    let mut engine = configured_rule_engine(&config);
                    engine.load(&project)?;
                    let violations = engine.scan(&project).await?;
                    // Persist rule scan results for observability/GC even when running via CLI.
                    match harness_observe::EventStore::with_policies(
                        &config.server.data_dir,
                        config.observe.session_renewal_secs,
                        config.observe.log_retention_days,
                    ) {
                        Ok(store) => {
                            store.persist_rule_scan(&project, &violations);
                        }
                        Err(e) => tracing::warn!(
                            "Failed to initialize event store, rule scan not persisted: {e}"
                        ),
                    }
                    if violations.is_empty() {
                        println!("No violations found");
                    } else {
                        for v in &violations {
                            println!(
                                "{:?} {}:{} [{}] {}",
                                v.severity,
                                v.file.display(),
                                v.line.unwrap_or(0),
                                v.rule_id,
                                v.message
                            );
                        }
                    }
                }
            }
        }

        Command::ExecPolicy { cmd } => match cmd {
            ExecPolicyCommand::Check {
                rules,
                requirements,
                resolve_host_executables,
                pretty,
                command,
            } => {
                let mut engine = configured_rule_engine(&config);
                let policy_paths = if rules.is_empty() {
                    config.rules.exec_policy_paths.clone()
                } else {
                    rules
                };
                if policy_paths.is_empty() {
                    anyhow::bail!(
                        "no execpolicy rules supplied; pass --rules or set rules.exec_policy_paths"
                    );
                }

                engine.load_exec_policy_files(&policy_paths)?;
                if let Some(path) = requirements {
                    engine.load_requirements_toml(&path)?;
                } else {
                    engine.load_configured_requirements()?;
                }

                let result = engine.check_command_policy(
                    &command,
                    &harness_rules::exec_policy::MatchOptions {
                        resolve_host_executables,
                    },
                );
                let rendered = if pretty {
                    serde_json::to_string_pretty(&result)?
                } else {
                    serde_json::to_string(&result)?
                };
                println!("{rendered}");
            }
        },

        Command::Skill { cmd } => match cmd {
            SkillCommand::List { query } => {
                let store = harness_skills::SkillStore::new();
                let skills = if let Some(q) = query {
                    store.search(&q).into_iter().cloned().collect::<Vec<_>>()
                } else {
                    store.list().to_vec()
                };
                for s in &skills {
                    println!("{}: {}", s.name, s.description);
                }
                if skills.is_empty() {
                    println!("No skills found");
                }
            }
            SkillCommand::Create { name, file } => {
                let content = std::fs::read_to_string(&file)?;
                let mut store = harness_skills::SkillStore::new();
                store.create(name.clone(), content);
                println!("Created skill: {name}");
            }
            SkillCommand::Delete { skill_id } => {
                println!("Delete skill: {skill_id}");
            }
        },

        Command::Pr { cmd } => match cmd {
            PrCommand::Fix { issue, args } => {
                crate::cmd::pr::fix(&config, issue, args.wait, args.max_rounds, args.project)
                    .await?;
            }
            PrCommand::Loop { pr, args } => {
                crate::cmd::pr::loop_pr(&config, pr, args.wait, args.max_rounds, args.project)
                    .await?;
            }
        },

        Command::Plan { cmd } => match cmd {
            PlanCommand::Init { spec } => {
                let content = std::fs::read_to_string(&spec)?;
                let project_root = std::env::current_dir()?;
                let plan = harness_exec::ExecPlan::from_spec(&content, &project_root)?;
                let md = plan.to_markdown();
                let out_path = format!("exec-plan-{}.md", plan.id);
                std::fs::write(&out_path, &md)?;
                println!("Created ExecPlan: {out_path}");
            }
            PlanCommand::Status { plan } => {
                if std::path::Path::new(&plan).exists() {
                    let content = std::fs::read_to_string(&plan)?;
                    let p = harness_exec::ExecPlan::from_markdown(&content)?;
                    println!("Plan: {}", p.purpose);
                    println!("Status: {:?}", p.status);
                    let done = p.progress.iter().filter(|m| m.completed).count();
                    println!("Progress: {}/{}", done, p.progress.len());
                } else {
                    println!("Plan file not found: {plan}");
                }
            }
        },
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;

    #[test]
    fn resolve_exec_project_root_prefers_explicit_project() {
        let explicit = PathBuf::from("/tmp/project");
        let resolved = resolve_exec_project_root_with(Some(explicit.clone()), || {
            panic!("current_dir fallback must not be used when --project is provided")
        })
        .expect("explicit project should resolve");

        assert_eq!(resolved, explicit);
    }

    #[test]
    fn resolve_exec_project_root_uses_current_dir_fallback() {
        let fallback = PathBuf::from("/tmp/fallback");
        let resolved = resolve_exec_project_root_with(None, || Ok(fallback.clone()))
            .expect("cwd fallback should resolve when current_dir succeeds");

        assert_eq!(resolved, fallback);
    }

    #[test]
    fn resolve_exec_project_root_returns_contextual_error_on_failure() {
        let error = resolve_exec_project_root_with(None, || {
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "cwd lookup blocked",
            ))
        })
        .expect_err("cwd failure should be returned as recoverable error");

        let message = error.to_string();
        assert!(message.contains(
            "failed to resolve `harness exec` project root from current working directory"
        ));
        let chain = error
            .chain()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(" | ");
        assert!(chain.contains("cwd lookup blocked"));
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
}
