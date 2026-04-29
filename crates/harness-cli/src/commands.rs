use chrono::{DateTime, NaiveDateTime, Utc};
use clap::{ArgAction, Args, Parser, Subcommand};
use harness_server::server::RuntimeLogMetadata;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tracing_subscriber::fmt::writer::MakeWriter;

mod exec;
mod reconcile;
mod serve;

const RUNTIME_LOG_PREFIX: &str = "harness-serve-";
const RUNTIME_LOG_SUFFIX: &str = ".log";

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

    /// Display the current version
    Version,

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
        /// Automatically apply fix_pattern replacements for violations that have one
        #[arg(long)]
        auto_fix: bool,
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
    #[arg(long, default_value = "8")]
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

fn configured_rule_engine(
    config: &harness_core::config::HarnessConfig,
) -> harness_rules::engine::RuleEngine {
    let mut engine = harness_rules::engine::RuleEngine::new();
    engine.configure_sources(
        config.rules.discovery_paths.clone(),
        config.rules.builtin_path.clone(),
        config.rules.requirements_path.clone(),
    );
    engine
}

fn configured_skill_store(
    config: &harness_core::config::HarnessConfig,
) -> anyhow::Result<harness_skills::store::SkillStore> {
    let project_root = std::env::current_dir()?;
    let mut store = harness_skills::store::SkillStore::new()
        .with_persist_dir(config.server.data_dir.join("skills"))
        .with_discovery(&project_root);
    store.load_builtin();
    store.discover()?;
    Ok(store)
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ConfigSource {
    Flag(PathBuf),
    Discovered(PathBuf),
    BuiltInDefaults,
}

#[derive(Clone)]
struct TeeMakeWriter {
    runtime_log_file: Option<Arc<Mutex<File>>>,
}

struct TeeWriter {
    stderr: io::Stderr,
    runtime_log_file: Option<Arc<Mutex<File>>>,
}

impl<'a> MakeWriter<'a> for TeeMakeWriter {
    type Writer = TeeWriter;

    fn make_writer(&'a self) -> Self::Writer {
        TeeWriter {
            stderr: io::stderr(),
            runtime_log_file: self.runtime_log_file.clone(),
        }
    }
}

impl Write for TeeWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.stderr.write_all(buf)?;
        if let Some(file) = &self.runtime_log_file {
            let mut guard = match file.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            guard.write_all(buf)?;
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.stderr.flush()?;
        if let Some(file) = &self.runtime_log_file {
            let mut guard = match file.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            guard.flush()?;
        }
        Ok(())
    }
}

struct LoggingBootstrap {
    runtime_logs: RuntimeLogMetadata,
    runtime_log_file: Option<Arc<Mutex<File>>>,
    setup_warning: Option<String>,
}

fn load_config(
    config_path: Option<&Path>,
) -> anyhow::Result<(harness_core::config::HarnessConfig, ConfigSource)> {
    if let Some(config_path) = config_path {
        let content = fs::read_to_string(config_path)?;
        let mut config: harness_core::config::HarnessConfig = toml::from_str(&content)?;
        if let Some(dir) = config_path.parent() {
            config.rebase_relative_paths(dir);
        }
        return Ok((config, ConfigSource::Flag(config_path.to_path_buf())));
    }

    if let Some(discovered) = harness_core::config::dirs::find_config_file() {
        let content = fs::read_to_string(&discovered)?;
        let mut config: harness_core::config::HarnessConfig = toml::from_str(&content)?;
        if let Some(dir) = discovered.parent() {
            config.rebase_relative_paths(dir);
        }
        return Ok((config, ConfigSource::Discovered(discovered)));
    }

    Ok((
        harness_core::config::HarnessConfig::default(),
        ConfigSource::BuiltInDefaults,
    ))
}

fn init_tracing(bootstrap: &LoggingBootstrap) -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_timer(tracing_subscriber::fmt::time::ChronoLocal::new(
            "%Y-%m-%dT%H:%M:%S%.3f%:z".to_string(),
        ))
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "harness=info,warn".into()),
        )
        .with_writer(TeeMakeWriter {
            runtime_log_file: bootstrap.runtime_log_file.clone(),
        })
        .try_init()
        .map_err(|error| anyhow::anyhow!("failed to initialize tracing subscriber: {error}"))
}

fn log_config_source(source: &ConfigSource) {
    match source {
        ConfigSource::Flag(path) => {
            tracing::info!("config loaded from --config flag: {}", path.display());
        }
        ConfigSource::Discovered(path) => {
            tracing::info!("config loaded from {}", path.display());
        }
        ConfigSource::BuiltInDefaults => {
            tracing::warn!("no config file found, using built-in defaults");
        }
    }
}

fn log_runtime_log_status(bootstrap: &LoggingBootstrap) {
    match bootstrap.runtime_logs.state {
        harness_server::server::RuntimeLogState::Enabled => {
            if let Some(path) = bootstrap.runtime_logs.active_path.as_ref() {
                tracing::info!(
                    path = %path.display(),
                    retention_days = bootstrap.runtime_logs.retention_days,
                    "runtime logs persisted to file"
                );
            }
        }
        harness_server::server::RuntimeLogState::Degraded => {
            tracing::warn!(
                path_hint = bootstrap
                    .runtime_logs
                    .path_hint
                    .as_deref()
                    .unwrap_or("logs"),
                retention_days = bootstrap.runtime_logs.retention_days,
                error = bootstrap
                    .setup_warning
                    .as_deref()
                    .unwrap_or("unknown setup error"),
                "runtime log persistence unavailable; continuing with console logging only"
            );
        }
        harness_server::server::RuntimeLogState::Disabled => {}
    }
}

fn prepare_logging(
    command: &Command,
    config: &harness_core::config::HarnessConfig,
) -> LoggingBootstrap {
    match command {
        Command::Serve { .. } => prepare_runtime_logs(config, Utc::now()),
        _ => LoggingBootstrap {
            runtime_logs: RuntimeLogMetadata::disabled(config.observe.log_retention_days),
            runtime_log_file: None,
            setup_warning: None,
        },
    }
}

fn prepare_runtime_logs(
    config: &harness_core::config::HarnessConfig,
    started_at: DateTime<Utc>,
) -> LoggingBootstrap {
    let retention_days = config.observe.log_retention_days;
    let log_path = runtime_log_path(&config.server.data_dir, started_at, std::process::id());
    let path_hint = RuntimeLogMetadata::public_path_hint(&log_path);

    match open_runtime_log_file(&log_path, retention_days, started_at) {
        Ok(file) => LoggingBootstrap {
            runtime_logs: RuntimeLogMetadata::enabled(log_path, retention_days),
            runtime_log_file: Some(Arc::new(Mutex::new(file))),
            setup_warning: None,
        },
        Err(error) => LoggingBootstrap {
            runtime_logs: RuntimeLogMetadata::degraded(Some(path_hint), retention_days),
            runtime_log_file: None,
            setup_warning: Some(error.to_string()),
        },
    }
}

fn runtime_log_path(data_dir: &Path, started_at: DateTime<Utc>, pid: u32) -> PathBuf {
    data_dir.join("logs").join(format!(
        "{RUNTIME_LOG_PREFIX}{}-pid{pid}{RUNTIME_LOG_SUFFIX}",
        started_at.format("%Y%m%dT%H%M%SZ")
    ))
}

fn open_runtime_log_file(
    log_path: &Path,
    retention_days: u32,
    started_at: DateTime<Utc>,
) -> io::Result<File> {
    let logs_dir = log_path
        .parent()
        .ok_or_else(|| io::Error::other("runtime log path missing parent directory"))?;
    purge_stale_runtime_logs(logs_dir, retention_days, started_at)?;
    fs::create_dir_all(logs_dir)?;
    OpenOptions::new().create(true).append(true).open(log_path)
}

fn purge_stale_runtime_logs(
    logs_dir: &Path,
    retention_days: u32,
    now: DateTime<Utc>,
) -> io::Result<()> {
    if !logs_dir.exists() {
        return Ok(());
    }

    let cutoff = now - chrono::Duration::days(i64::from(retention_days));
    for entry in fs::read_dir(logs_dir)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }

        let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let Some(started_at) = parse_runtime_log_started_at(file_name) else {
            continue;
        };
        if started_at < cutoff {
            fs::remove_file(path)?;
        }
    }

    Ok(())
}

fn parse_runtime_log_started_at(file_name: &str) -> Option<DateTime<Utc>> {
    let trimmed = file_name
        .strip_prefix(RUNTIME_LOG_PREFIX)?
        .strip_suffix(RUNTIME_LOG_SUFFIX)?;
    let (timestamp, _) = trimmed.rsplit_once("-pid")?;
    let naive = NaiveDateTime::parse_from_str(timestamp, "%Y%m%dT%H%M%SZ").ok()?;
    Some(DateTime::from_naive_utc_and_offset(naive, Utc))
}

pub async fn run(cli: Cli) -> anyhow::Result<()> {
    let (mut config, config_source) = load_config(cli.config.as_deref())?;
    // Apply env var overrides for all subcommands so that HARNESS_DATA_DIR,
    // HARNESS_PROJECT_ROOT, etc. are respected by gc, rule check, and skill
    // commands — not just `serve`.
    config.apply_env_overrides()?;
    harness_core::db::configure_pg_pool_from_server(&config.server);
    let logging = prepare_logging(&cli.command, &config);
    init_tracing(&logging)?;
    log_config_source(&config_source);
    log_runtime_log_status(&logging);

    match cli.command {
        Command::Serve {
            transport,
            port,
            project_root,
            projects,
            default_project,
        } => {
            serve::run(
                config,
                transport,
                port,
                project_root,
                projects,
                default_project,
                logging.runtime_logs.clone(),
            )
            .await?;
        }

        Command::McpServer => {
            crate::cmd::mcp_server::run(config.clone()).await?;
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
            crate::gc::run_gc(cmd, &config).await?;
        }

        Command::Rule { cmd } => {
            match cmd {
                RuleCommand::Load { project } => {
                    let mut engine = configured_rule_engine(&config);
                    engine.load(&project)?;
                    println!("Loaded {} rules", engine.rules().len());
                }
                RuleCommand::Check { project, auto_fix } => {
                    let mut engine = configured_rule_engine(&config);
                    engine.load(&project)?;
                    let violations = engine.scan(&project).await?;
                    // Persist rule scan results for observability/GC even when running via CLI.
                    match harness_observe::event_store::EventStore::with_policies_and_otel_with_database_url(
                        &config.server.data_dir,
                        config.server.database_url.as_deref(),
                        config.observe.session_renewal_secs,
                        config.observe.log_retention_days,
                        &config.otel,
                    )
                    .await
                    {
                        Ok(store) => {
                            store.persist_rule_scan(&project, &violations).await;
                            store.shutdown().await;
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
                        if auto_fix {
                            let fixed = engine.apply_fixes(&violations, &project)?;
                            println!("Auto-fixed {fixed} file(s)");
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
                let store = configured_skill_store(&config)?;
                let skills = if let Some(q) = query {
                    store.search(&q).into_iter().cloned().collect::<Vec<_>>()
                } else {
                    store.list().to_vec()
                };
                for s in &skills {
                    println!("{} [{}]: {}", s.name, s.id, s.description);
                }
                if skills.is_empty() {
                    println!("No skills found");
                }
            }
            SkillCommand::Create { name, file } => {
                let content = std::fs::read_to_string(&file)?;
                let mut store = configured_skill_store(&config)?;
                store.create(name.clone(), content);
                println!("Created skill: {name}");
            }
            SkillCommand::Delete { skill_id } => {
                let mut store = configured_skill_store(&config)?;
                let deleted = if let Some(skill) = store.get_by_name(&skill_id).cloned() {
                    store.delete(&skill.id)
                } else {
                    store.delete(&harness_core::types::SkillId::from_str(&skill_id))
                };
                println!("Deleted skill: {deleted}");
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
                let plan = harness_exec::plan::ExecPlan::from_spec(&content, &project_root)?;
                let md = plan.to_markdown();
                let out_path = format!("exec-plan-{}.md", plan.id);
                std::fs::write(&out_path, &md)?;
                println!("Created ExecPlan: {out_path}");
            }
            PlanCommand::Status { plan } => {
                if std::path::Path::new(&plan).exists() {
                    let content = std::fs::read_to_string(&plan)?;
                    let p = harness_exec::plan::ExecPlan::from_markdown(&content)?;
                    println!("Plan: {}", p.purpose);
                    println!("Status: {:?}", p.status);
                    let done = p.progress.iter().filter(|m| m.completed).count();
                    println!("Progress: {}/{}", done, p.progress.len());
                } else {
                    println!("Plan file not found: {plan}");
                }
            }
        },

        Command::Version => {
            let cargo_toml_path = std::env::current_dir()?.join("Cargo.toml");
            if !cargo_toml_path.exists() {
                anyhow::bail!("Cargo.toml not found in current directory");
            }
            let content = std::fs::read_to_string(&cargo_toml_path)?;
            let parsed: toml::Value = toml::from_str(&content)?;

            if let Some(version) = parsed
                .get("workspace")
                .and_then(|w| w.get("package"))
                .and_then(|p| p.get("version"))
                .and_then(|v| v.as_str())
            {
                println!("Current version: {}", version);
            } else if let Some(version) = parsed
                .get("package")
                .and_then(|p| p.get("version"))
                .and_then(|v| v.as_str())
            {
                println!("Current version: {}", version);
            } else {
                anyhow::bail!("Version field not found in Cargo.toml");
            }
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
    use crate::commands::exec::{
        apply_sandbox_hint, current_username, enforce_exec_actor_filters,
        enforce_exec_privilege_policy_with, normalize_allow_list, resolve_exec_output_path,
        ExecSandboxMode,
    };
    use chrono::TimeZone;
    use std::path::Path;

    #[test]
    fn sandbox_mode_parse_accepts_supported_values() {
        assert_eq!(
            ExecSandboxMode::parse("read-only").expect("read-only should parse"),
            ExecSandboxMode::ReadOnly
        );
        assert_eq!(
            ExecSandboxMode::parse("workspace-write").expect("workspace-write should parse"),
            ExecSandboxMode::WorkspaceWrite
        );
        assert_eq!(
            ExecSandboxMode::parse("danger-full-access").expect("danger-full-access should parse"),
            ExecSandboxMode::DangerFullAccess
        );
    }

    #[test]
    fn sandbox_mode_parse_rejects_unknown_value() {
        let error = ExecSandboxMode::parse("unsafe").expect_err("unsupported mode should fail");
        assert!(error
            .to_string()
            .contains("unsupported sandbox mode `unsafe`"));
    }

    #[test]
    fn normalize_allow_list_trims_and_drops_empty_entries() {
        let values = vec![
            "alice".to_string(),
            "  bob  ".to_string(),
            "".to_string(),
            "   ".to_string(),
        ];
        assert_eq!(normalize_allow_list(values), vec!["alice", "bob"]);
    }

    #[test]
    fn exec_actor_filters_allow_matching_user_or_bot() {
        let users = vec!["alice".to_string()];
        let bots = vec!["dependabot[bot]".to_string()];

        enforce_exec_actor_filters(Some("alice".to_string()), &users, &bots)
            .expect("listed human user should pass");
        enforce_exec_actor_filters(Some("dependabot[bot]".to_string()), &users, &bots)
            .expect("listed bot should pass");
    }

    #[test]
    fn exec_actor_filters_block_unlisted_actor() {
        let users = vec!["alice".to_string()];
        let bots = vec!["dependabot[bot]".to_string()];
        let error = enforce_exec_actor_filters(Some("mallory".to_string()), &users, &bots)
            .expect_err("unlisted actor should be rejected when allow lists are configured");

        assert!(error
            .to_string()
            .contains("actor `mallory` is not allowed to run `harness exec`"));
    }

    #[test]
    fn apply_sandbox_hint_prefixes_prompt() {
        let prompt = "review this PR".to_string();
        let hinted = apply_sandbox_hint(prompt.clone(), ExecSandboxMode::WorkspaceWrite);
        assert!(hinted.contains("Sandbox mode requirement for this run: `workspace-write`."));
        assert!(hinted.ends_with(&prompt));
    }

    #[test]
    fn resolve_exec_output_path_accepts_nested_relative_path() {
        let root = std::env::temp_dir().join("harness-cli-output-path-accept");
        std::fs::create_dir_all(&root).expect("temp test root should be creatable");

        let output = resolve_exec_output_path(&root, Path::new(".harness/final.txt"))
            .expect("relative output file should resolve inside project root");

        assert!(output.starts_with(root.canonicalize().expect("root should canonicalize")));
        assert!(output.ends_with(Path::new(".harness/final.txt")));
    }

    #[test]
    fn resolve_exec_output_path_rejects_parent_escape() {
        let root = std::env::temp_dir().join("harness-cli-output-path-reject");
        std::fs::create_dir_all(&root).expect("temp test root should be creatable");

        let error = resolve_exec_output_path(&root, Path::new("../escape.txt"))
            .expect_err("path traversal outside project root should fail");

        assert!(error
            .to_string()
            .contains("`--output-file` must stay within project root"));
    }

    #[cfg(unix)]
    #[test]
    fn resolve_exec_output_path_rejects_symlink_escape() {
        use std::os::unix::fs::symlink;

        let suffix = std::process::id();
        let root =
            std::env::temp_dir().join(format!("harness-cli-output-path-symlink-root-{suffix}"));
        let outside =
            std::env::temp_dir().join(format!("harness-cli-output-path-symlink-outside-{suffix}"));
        let link = root.join("escape-link");

        std::fs::create_dir_all(&root).expect("temp root should be creatable");
        std::fs::create_dir_all(&outside).expect("outside dir should be creatable");
        if link.exists() {
            std::fs::remove_file(&link).expect("pre-existing symlink should be removable");
        }
        symlink(&outside, &link).expect("symlink should be creatable");

        let error = resolve_exec_output_path(&root, Path::new("escape-link/evil.txt"))
            .expect_err("symlink-based escape should be rejected");

        assert!(error
            .to_string()
            .contains("`--output-file` must stay within project root"));

        let _ = std::fs::remove_file(&link);
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&outside);
    }

    #[test]
    fn enforce_exec_privilege_policy_blocks_root_when_drop_sudo_enabled() {
        let error = enforce_exec_privilege_policy_with(true, None, || true, || false, || None)
            .expect_err("drop-sudo should reject execution when real UID indicates root");

        assert!(error
            .to_string()
            .contains("refusing to run `harness exec` with elevated privileges"));
    }

    #[test]
    fn enforce_exec_privilege_policy_allows_root_when_drop_sudo_disabled() {
        enforce_exec_privilege_policy_with(false, None, || true, || false, || None)
            .expect("drop-sudo=false should allow root execution when explicitly requested");
    }

    #[test]
    fn enforce_exec_privilege_policy_blocks_sudo_environment() {
        let error = enforce_exec_privilege_policy_with(true, None, || false, || true, || None)
            .expect_err(
                "drop-sudo should reject execution when sudo environment markers are present",
            );

        assert!(error
            .to_string()
            .contains("refusing to run `harness exec` with elevated privileges"));
    }

    #[test]
    fn enforce_exec_privilege_policy_blocks_unexpected_user() {
        let error = enforce_exec_privilege_policy_with(
            false,
            Some("runner"),
            || false,
            || false,
            || Some("root".to_string()),
        )
        .expect_err("mismatched --unprivileged-user should fail");

        assert!(error
            .to_string()
            .contains("`harness exec` must run as `runner`, current user is `root`"));
    }

    #[test]
    fn enforce_exec_privilege_policy_allows_expected_user() {
        enforce_exec_privilege_policy_with(
            false,
            Some("runner"),
            || false,
            || false,
            || Some("runner".to_string()),
        )
        .expect("matching --unprivileged-user should pass");
    }

    #[cfg(unix)]
    #[test]
    fn current_username_uses_real_uid_lookup() {
        use std::ffi::CStr;

        let uid = unsafe { libc::getuid() };
        let passwd = unsafe { libc::getpwuid(uid) };
        assert!(
            !passwd.is_null(),
            "getpwuid(getuid()) should resolve current user"
        );

        let username_ptr = unsafe { (*passwd).pw_name };
        assert!(
            !username_ptr.is_null(),
            "passwd record should contain pw_name"
        );

        let expected = unsafe { CStr::from_ptr(username_ptr) }
            .to_str()
            .expect("pw_name should be valid UTF-8")
            .trim()
            .to_string();

        let actual = current_username().expect("current_username should resolve via UID lookup");
        assert_eq!(actual, expected);
    }

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

    #[test]
    fn prepare_logging_creates_runtime_log_for_serve() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let mut config = harness_core::config::HarnessConfig::default();
        config.server.data_dir = tempdir.path().to_path_buf();

        let bootstrap = prepare_logging(
            &Command::Serve {
                transport: None,
                port: None,
                project_root: None,
                projects: vec![],
                default_project: None,
            },
            &config,
        );

        assert_eq!(bootstrap.runtime_logs.state.as_str(), "enabled");
        let active_path = bootstrap
            .runtime_logs
            .active_path
            .as_ref()
            .expect("active path");
        assert!(active_path.starts_with(tempdir.path().join("logs")));
        assert!(active_path.exists(), "runtime log file should be created");
        assert_eq!(
            bootstrap.runtime_logs.path_hint.as_deref(),
            active_path
                .file_name()
                .and_then(|name| name.to_str())
                .map(|name| format!("logs/{name}"))
                .as_deref()
        );
    }

    #[test]
    fn purge_stale_runtime_logs_keeps_current_and_non_matching_files() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let logs_dir = tempdir.path().join("logs");
        fs::create_dir_all(&logs_dir).expect("create logs dir");
        let now = Utc
            .with_ymd_and_hms(2026, 4, 30, 12, 0, 0)
            .single()
            .expect("timestamp");
        let stale = runtime_log_path(
            tempdir.path(),
            Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0)
                .single()
                .expect("stale timestamp"),
            10,
        );
        let fresh = runtime_log_path(
            tempdir.path(),
            Utc.with_ymd_and_hms(2026, 4, 29, 12, 0, 0)
                .single()
                .expect("fresh timestamp"),
            11,
        );
        let unrelated = logs_dir.join("notes.txt");
        fs::write(&stale, "stale").expect("write stale");
        fs::write(&fresh, "fresh").expect("write fresh");
        fs::write(&unrelated, "keep").expect("write unrelated");

        purge_stale_runtime_logs(&logs_dir, 30, now).expect("purge");

        assert!(!stale.exists(), "stale runtime log should be deleted");
        assert!(fresh.exists(), "fresh runtime log should be kept");
        assert!(unrelated.exists(), "non-matching files should be kept");
    }

    #[test]
    fn prepare_logging_disables_file_persistence_for_non_serve_commands() {
        let config = harness_core::config::HarnessConfig::default();
        let bootstrap = prepare_logging(&Command::Version, &config);

        assert_eq!(bootstrap.runtime_logs.state.as_str(), "disabled");
        assert!(bootstrap.runtime_logs.active_path.is_none());
        assert!(bootstrap.runtime_log_file.is_none());
    }
}
