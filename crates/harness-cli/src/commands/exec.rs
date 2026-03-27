use anyhow::{Context, Result};
use harness_core::HarnessConfig;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecSandboxMode {
    ReadOnly,
    WorkspaceWrite,
    DangerFullAccess,
}

impl ExecSandboxMode {
    pub fn parse(input: &str) -> Result<Self> {
        match input {
            "read-only" => Ok(Self::ReadOnly),
            "workspace-write" => Ok(Self::WorkspaceWrite),
            "danger-full-access" => Ok(Self::DangerFullAccess),
            other => anyhow::bail!(
                "unsupported sandbox mode `{other}`; expected one of: read-only, workspace-write, danger-full-access"
            ),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "read-only",
            Self::WorkspaceWrite => "workspace-write",
            Self::DangerFullAccess => "danger-full-access",
        }
    }

    pub fn to_sandbox_mode(self) -> harness_core::SandboxMode {
        match self {
            Self::ReadOnly => harness_core::SandboxMode::ReadOnly,
            Self::WorkspaceWrite => harness_core::SandboxMode::WorkspaceWrite,
            Self::DangerFullAccess => harness_core::SandboxMode::DangerFullAccess,
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

pub fn normalize_allow_list(values: Vec<String>) -> Vec<String> {
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

pub fn enforce_exec_actor_filters(
    actor: Option<String>,
    allow_users: &[String],
    allow_bots: &[String],
) -> Result<()> {
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

pub fn current_username() -> Option<String> {
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

pub fn enforce_exec_privilege_policy_with<IsRootFn, HasSudoEnvFn, CurrentUserFn>(
    drop_sudo: bool,
    unprivileged_user: Option<&str>,
    is_root_fn: IsRootFn,
    has_sudo_env_fn: HasSudoEnvFn,
    current_user_fn: CurrentUserFn,
) -> Result<()>
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

pub fn enforce_exec_privilege_policy(
    drop_sudo: bool,
    unprivileged_user: Option<&str>,
) -> Result<()> {
    enforce_exec_privilege_policy_with(
        drop_sudo,
        unprivileged_user,
        effective_uid_is_root,
        detect_sudo_environment,
        current_username,
    )
}

fn normalize_absolute_output_path(path: &Path) -> Result<PathBuf> {
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

pub fn resolve_exec_output_path(project_root: &Path, output_file: &Path) -> Result<PathBuf> {
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

pub fn apply_sandbox_hint(prompt: String, sandbox_mode: ExecSandboxMode) -> String {
    format!(
        "Sandbox mode requirement for this run: `{}`.\n\n{}",
        sandbox_mode.as_str(),
        prompt
    )
}

pub fn resolve_exec_project_root(project: Option<PathBuf>) -> Result<PathBuf> {
    resolve_exec_project_root_with(project, std::env::current_dir)
}

pub fn resolve_exec_project_root_with<F>(
    project: Option<PathBuf>,
    current_dir: F,
) -> Result<PathBuf>
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

#[allow(clippy::too_many_arguments)]
pub async fn run(
    config: HarnessConfig,
    prompt: String,
    project: Option<PathBuf>,
    agent: String,
    model: Option<String>,
    sandbox_mode: String,
    output_file: Option<PathBuf>,
    drop_sudo: bool,
    unprivileged_user: Option<String>,
    allow_users: Vec<String>,
    allow_bots: Vec<String>,
    actor: Option<String>,
) -> Result<()> {
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
    let runtime_sandbox_mode = sandbox_mode.to_sandbox_mode();

    let req = harness_core::AgentRequest {
        prompt: apply_sandbox_hint(prompt, sandbox_mode),
        project_root: project_root.clone(),
        model,
        ..Default::default()
    };

    let mut agent_registry = harness_agents::AgentRegistry::new(&config.agents.default_agent);
    agent_registry.set_complexity_preferences(config.agents.complexity_preferred_agents.clone());
    agent_registry.register(
        "claude",
        Arc::new(
            harness_agents::claude::ClaudeCodeAgent::new(
                config.agents.claude.cli_path.clone(),
                config.agents.claude.default_model.clone(),
                runtime_sandbox_mode,
            )
            .with_stream_timeout(config.agents.stream_timeout_secs),
        ),
    );
    agent_registry.register(
        "codex",
        Arc::new(
            harness_agents::codex::CodexAgent::from_config(
                config.agents.codex.clone(),
                runtime_sandbox_mode,
            )
            .with_stream_timeout(config.agents.stream_timeout_secs),
        ),
    );
    if let Ok(api_key) = std::env::var("ANTHROPIC_API_KEY") {
        agent_registry.register(
            "anthropic-api",
            Arc::new(
                harness_agents::anthropic_api::AnthropicApiAgent::from_config(
                    api_key,
                    &config.agents.anthropic_api,
                ),
            ),
        );
    }

    let selected_agent = if agent.eq_ignore_ascii_case("auto") {
        agent_registry.default_agent()
    } else {
        agent_registry.get(&agent)
    }
    .ok_or_else(|| {
        anyhow::anyhow!(
            "unknown exec agent `{agent}`; supported values are: {}",
            agent_registry.list().join(", ")
        )
    })?;

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
    }
}
