use anyhow::{Context, Result};
use harness_core::config::HarnessConfig;
use std::path::{Component, Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecSandboxMode {
    ReadOnly,
    ReadOnlyWithNetwork,
    WorkspaceWrite,
    DangerFullAccess,
}

impl ExecSandboxMode {
    pub fn parse(input: &str) -> Result<Self> {
        match input {
            "read-only" => Ok(Self::ReadOnly),
            "read-only-with-network" => Ok(Self::ReadOnlyWithNetwork),
            "workspace-write" => Ok(Self::WorkspaceWrite),
            "danger-full-access" => Ok(Self::DangerFullAccess),
            other => anyhow::bail!(
                "unsupported sandbox mode `{other}`; expected one of: read-only, read-only-with-network, workspace-write, danger-full-access"
            ),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "read-only",
            Self::ReadOnlyWithNetwork => "read-only-with-network",
            Self::WorkspaceWrite => "workspace-write",
            Self::DangerFullAccess => "danger-full-access",
        }
    }

    pub fn to_sandbox_mode(self) -> harness_core::config::agents::SandboxMode {
        match self {
            Self::ReadOnly => harness_core::config::agents::SandboxMode::ReadOnly,
            Self::ReadOnlyWithNetwork => {
                harness_core::config::agents::SandboxMode::ReadOnlyWithNetwork
            }
            Self::WorkspaceWrite => harness_core::config::agents::SandboxMode::WorkspaceWrite,
            Self::DangerFullAccess => harness_core::config::agents::SandboxMode::DangerFullAccess,
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

    let mut req = harness_core::agent::AgentRequest {
        prompt: apply_sandbox_hint(prompt, sandbox_mode),
        project_root: project_root.clone(),
        model,
        ..Default::default()
    };
    req.apply_configured_policy(&config);

    // `exec` resolves the sandbox mode from its own flags, so it passes that
    // rather than `config.agents.sandbox_mode`.
    let agent_registry =
        harness_agents::builder::registry_from_config(&config.agents, runtime_sandbox_mode)?;

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
            ExecSandboxMode::parse("read-only-with-network")
                .expect("read-only-with-network should parse"),
            ExecSandboxMode::ReadOnlyWithNetwork
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
}
