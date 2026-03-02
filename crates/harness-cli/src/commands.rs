use clap::{Parser, Subcommand};
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
    },

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
    Drafts {
        project: Option<PathBuf>,
    },
    /// Adopt a draft
    Adopt {
        draft_id: String,
    },
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
    Delete {
        skill_id: String,
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

pub async fn run(cli: Cli) -> anyhow::Result<()> {
    // Load config
    let config = if let Some(config_path) = &cli.config {
        let content = std::fs::read_to_string(config_path)?;
        toml::from_str(&content)?
    } else {
        harness_core::HarnessConfig::default()
    };

    match cli.command {
        Command::Serve { transport, port } => {
            let thread_manager = harness_server::thread_manager::ThreadManager::new();
            let agent_registry = harness_agents::AgentRegistry::new(&config.agents.default_agent);
            let server = harness_server::server::HarnessServer::new(
                config.clone(),
                thread_manager,
                agent_registry,
            );

            match transport.as_str() {
                "stdio" => server.serve_stdio().await?,
                "http" => {
                    let addr = if let Some(p) = port {
                        format!("127.0.0.1:{p}").parse()?
                    } else {
                        config.server.http_addr
                    };
                    Arc::new(server).serve_http(addr).await?
                }
                other => anyhow::bail!("unknown transport: {other}"),
            }
        }

        Command::Exec { prompt, project, agent: _agent_name } => {
            let project_root = project.unwrap_or_else(|| std::env::current_dir().unwrap());
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
        }

        Command::Gc { cmd } => {
            crate::gc::run_gc(cmd, &config).await?;
        }

        Command::Rule { cmd } => {
            match cmd {
                RuleCommand::Load { project } => {
                    let mut engine = harness_rules::RuleEngine::new();
                    engine.load(&project)?;
                    println!("Loaded {} rules", engine.rules().len());
                }
                RuleCommand::Check { project } => {
                    let mut engine = harness_rules::RuleEngine::new();
                    engine.load(&project)?;
                    let violations = engine.scan(&project).await?;
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

        Command::Skill { cmd } => {
            match cmd {
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
            }
        }

        Command::Plan { cmd } => {
            match cmd {
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
            }
        }
    }

    Ok(())
}
