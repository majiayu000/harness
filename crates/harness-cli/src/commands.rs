use anyhow::Context;
use clap::{ArgAction, Args, Parser, Subcommand};
use std::path::{Component, Path, PathBuf};
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExecSandboxMode {
    ReadOnly,
    WorkspaceWrite,
    DangerFullAccess,
}

impl ExecSandboxMode {
    fn parse(input: &str) -> anyhow::Result<Self> {
        match input {
            "read-only" => Ok(Self::ReadOnly),
            "workspace-write" => Ok(Self::WorkspaceWrite),
            "danger-full-access" => Ok(Self::DangerFullAccess),
            other => anyhow::bail!(
                "unsupported sandbox mode `{other}`; expected one of: read-only, workspace-write, danger-full-access"
            ),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "read-only",
            Self::WorkspaceWrite => "workspace-write",
            Self::DangerFullAccess => "danger-full-access",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GitHubActorKind {
    User,
    Bot,
}

fn classify_github_actor(actor: &str) -> GitHubActorKind {
    if actor.ends_with("[bot]") {
        GitHubActorKind::Bot
    } else {
        GitHubActorKind::User
    }
}

fn normalize_allow_list(values: Vec<String>) -> Vec<String> {
    values
        .into_iter()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .collect()
}

fn allow_list_matches(list: &[String], actor: &str) -> bool {
    list.iter().any(|entry| entry == "*" || entry == actor)
}

fn resolve_exec_actor(actor: Option<String>) -> Option<String> {
    actor
        .or_else(|| std::env::var("GITHUB_ACTOR").ok())
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn enforce_exec_actor_filters(
    actor: Option<String>,
    allow_users: &[String],
    allow_bots: &[String],
) -> anyhow::Result<()> {
    if allow_users.is_empty() && allow_bots.is_empty() {
        return Ok(());
    }

    let actor = resolve_exec_actor(actor).ok_or_else(|| {
        anyhow::anyhow!(
            "allow lists are configured but no actor identity was provided; pass --actor or set GITHUB_ACTOR"
        )
    })?;

    let allowed = match classify_github_actor(&actor) {
        GitHubActorKind::User => allow_list_matches(allow_users, &actor),
        GitHubActorKind::Bot => allow_list_matches(allow_bots, &actor),
    };

    if !allowed {
        anyhow::bail!("actor `{actor}` is not allowed to run `harness exec`");
    }

    Ok(())
}

fn current_username() -> Option<String> {
    #[cfg(unix)]
    {
        use std::ffi::CStr;

        let uid = unsafe { libc::getuid() };
        let passwd_ptr = unsafe { libc::getpwuid(uid) };

        return unsafe {
            passwd_ptr
                .as_ref()
                .and_then(|passwd| passwd.pw_name.as_ref())
                .and_then(|name| CStr::from_ptr(name).to_str().ok())
        }
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned);
    }

    #[cfg(not(unix))]
    {
        None
    }
}

fn effective_uid_is_root() -> bool {
    #[cfg(unix)]
    {
        unsafe { libc::geteuid() == 0 }
    }

    #[cfg(not(unix))]
    {
        false
    }
}

fn detect_sudo_environment() -> bool {
    std::env::var_os("SUDO_UID").is_some() || std::env::var_os("SUDO_USER").is_some()
}

fn enforce_exec_privilege_policy_with<IsRootFn, HasSudoEnvFn, CurrentUserFn>(
    drop_sudo: bool,
    unprivileged_user: Option<&str>,
    is_root_fn: IsRootFn,
    has_sudo_env_fn: HasSudoEnvFn,
    current_user_fn: CurrentUserFn,
) -> anyhow::Result<()>
where
    IsRootFn: FnOnce() -> bool,
    HasSudoEnvFn: FnOnce() -> bool,
    CurrentUserFn: FnOnce() -> Option<String>,
{
    let is_root_user = is_root_fn();
    let has_sudo_env = has_sudo_env_fn();

    if drop_sudo && (is_root_user || has_sudo_env) {
        anyhow::bail!(
            "refusing to run `harness exec` with elevated privileges; rerun without sudo or pass --drop-sudo=false"
        );
    }

    if let Some(expected_user) = unprivileged_user {
        let expected_user = expected_user.trim();
        if !expected_user.is_empty() {
            let current_user = current_user_fn().ok_or_else(|| {
                anyhow::anyhow!("unable to determine current OS user for --unprivileged-user check")
            })?;
            if current_user != expected_user {
                anyhow::bail!(
                    "`harness exec` must run as `{expected_user}`, current user is `{current_user}`"
                );
            }
        }
    }

    Ok(())
}

fn enforce_exec_privilege_policy(
    drop_sudo: bool,
    unprivileged_user: Option<&str>,
) -> anyhow::Result<()> {
    enforce_exec_privilege_policy_with(
        drop_sudo,
        unprivileged_user,
        effective_uid_is_root,
        detect_sudo_environment,
        current_username,
    )
}

fn normalize_absolute_output_path(path: &Path) -> anyhow::Result<PathBuf> {
    if !path.is_absolute() {
        anyhow::bail!("expected absolute path when normalizing `--output-file`");
    }

    let mut normalized = PathBuf::new();
    let mut saw_normal_component = false;

    for component in path.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(component.as_os_str()),
            Component::CurDir => {}
            Component::Normal(segment) => {
                saw_normal_component = true;
                normalized.push(segment);
            }
            Component::ParentDir => {
                if !normalized.pop() {
                    anyhow::bail!("`--output-file` cannot escape the project root");
                }
            }
        }
    }

    if !saw_normal_component {
        anyhow::bail!("`--output-file` must reference a file path within project root");
    }

    Ok(normalized)
}

fn nearest_existing_ancestor(path: &Path) -> Option<&Path> {
    let mut candidate = path;
    loop {
        if candidate.exists() {
            return Some(candidate);
        }
        candidate = candidate.parent()?;
    }
}

fn resolve_exec_output_path(project_root: &Path, output_file: &Path) -> anyhow::Result<PathBuf> {
    let canonical_project_root = project_root.canonicalize().with_context(|| {
        format!(
            "failed to canonicalize project root {} for --output-file validation",
            project_root.display()
        )
    })?;

    let candidate_input = if output_file.is_absolute() {
        output_file.to_path_buf()
    } else {
        canonical_project_root.join(output_file)
    };

    let candidate = normalize_absolute_output_path(&candidate_input)?;

    if !candidate.starts_with(&canonical_project_root) {
        anyhow::bail!(
            "`--output-file` must stay within project root `{}`",
            canonical_project_root.display()
        );
    }

    let parent = candidate.parent().ok_or_else(|| {
        anyhow::anyhow!("`--output-file` must include a valid parent path within project root")
    })?;
    let existing_ancestor = nearest_existing_ancestor(parent).ok_or_else(|| {
        anyhow::anyhow!("unable to resolve existing ancestor for `--output-file` validation")
    })?;
    let canonical_ancestor = existing_ancestor.canonicalize().with_context(|| {
        format!(
            "failed to canonicalize output ancestor {} for --output-file validation",
            existing_ancestor.display()
        )
    })?;
    if !canonical_ancestor.starts_with(&canonical_project_root) {
        anyhow::bail!(
            "`--output-file` must stay within project root `{}`",
            canonical_project_root.display()
        );
    }

    Ok(candidate)
}

fn apply_sandbox_hint(prompt: String, sandbox_mode: ExecSandboxMode) -> String {
    format!(
        "Sandbox mode requirement for this run: `{}`.\n\n{}",
        sandbox_mode.as_str(),
        prompt
    )
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
                Arc::new(harness_agents::codex::CodexAgent::from_config(
                    serve_config.agents.codex.clone(),
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
            let project_root = resolve_exec_project_root(project)?;
            let sandbox_mode = ExecSandboxMode::parse(&sandbox_mode)?;
            let allow_users = normalize_allow_list(allow_users);
            let allow_bots = normalize_allow_list(allow_bots);
            let output_path = output_file
                .as_deref()
                .map(|path| resolve_exec_output_path(&project_root, path))
                .transpose()?;

            enforce_exec_actor_filters(actor, &allow_users, &allow_bots)?;
            enforce_exec_privilege_policy(drop_sudo, unprivileged_user.as_deref())?;

            let req = harness_core::AgentRequest {
                prompt: apply_sandbox_hint(prompt, sandbox_mode),
                project_root: project_root.clone(),
                model,
                ..Default::default()
            };

            let selected_agent: Arc<dyn harness_core::CodeAgent> = match agent.as_str() {
                "claude" => Arc::new(harness_agents::claude::ClaudeCodeAgent::new(
                    config.agents.claude.cli_path.clone(),
                    config.agents.claude.default_model.clone(),
                )),
                "codex" => Arc::new(harness_agents::codex::CodexAgent::new(
                    config.agents.codex.cli_path.clone(),
                )),
                other => anyhow::bail!(
                    "unknown exec agent `{other}`; supported values are: claude, codex"
                ),
            };

            let resp = selected_agent.execute(req).await?;
            if let Some(output_path) = output_path {
                if let Some(parent) = output_path.parent() {
                    if !parent.as_os_str().is_empty() {
                        std::fs::create_dir_all(parent).with_context(|| {
                            format!(
                                "failed to create parent directory for output file {}",
                                output_path.display()
                            )
                        })?;
                    }
                }
                std::fs::write(&output_path, &resp.output).with_context(|| {
                    format!(
                        "failed to write `harness exec` output to {}",
                        output_path.display()
                    )
                })?;
            }

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
                    match harness_observe::EventStore::with_policies_and_otel(
                        &config.server.data_dir,
                        config.observe.session_renewal_secs,
                        config.observe.log_retention_days,
                        &config.otel,
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
        let root = std::env::temp_dir().join(format!("harness-cli-output-path-symlink-root-{suffix}"));
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
        assert!(!passwd.is_null(), "getpwuid(getuid()) should resolve current user");

        let username_ptr = unsafe { (*passwd).pw_name };
        assert!(!username_ptr.is_null(), "passwd record should contain pw_name");

        let expected = unsafe { CStr::from_ptr(username_ptr) }
            .to_str()
            .expect("pw_name should be valid UTF-8")
            .trim()
            .to_string();

        let actual = current_username().expect("current_username should resolve via UID lookup");
        assert_eq!(actual, expected);
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
