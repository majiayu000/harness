use super::*;

pub(super) fn non_empty(value: String, field: &str) -> Result<String, ManifestError> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(ManifestError::new(format!("{field} must not be empty")));
    }
    if trimmed.len() == value.len() {
        Ok(value)
    } else {
        Ok(trimmed.to_string())
    }
}

pub(super) fn validate_repo(repo: &str) -> Result<(), ManifestError> {
    let Some((owner, name)) = repo.split_once('/') else {
        return Err(ManifestError::new(format!(
            "repo must use owner/name syntax: {repo}"
        )));
    };
    if owner.is_empty()
        || name.is_empty()
        || name.contains('/')
        || repo.chars().any(char::is_whitespace)
    {
        return Err(ManifestError::new(format!(
            "repo must use owner/name syntax: {repo}"
        )));
    }
    Ok(())
}

pub(super) fn validate_base_commit(base_commit: &str) -> Result<(), ManifestError> {
    let len = base_commit.len();
    if !(7..=40).contains(&len) || !base_commit.chars().all(|ch| ch.is_ascii_hexdigit()) {
        return Err(ManifestError::new(format!(
            "base_commit must be a 7 to 40 character hex commit: {base_commit}"
        )));
    }
    Ok(())
}

pub(super) fn normalize_verify_commands(
    verify_commands: Vec<String>,
    mode: EvalVerifyCommandMode,
    case_index: usize,
) -> Result<Vec<String>, ManifestError> {
    if verify_commands.is_empty() {
        return Err(ManifestError::new(format!(
            "case {} must include at least one verify command",
            case_index + 1
        )));
    }
    verify_commands
        .into_iter()
        .map(|command| {
            let command = non_empty(command, "verify command")?;
            validate_command_structure(&command)?;
            if mode == EvalVerifyCommandMode::Argv {
                let argv = shlex::split(&command).ok_or_else(|| {
                    ManifestError::new(format!(
                        "case {} verify command has invalid quoting: {command}",
                        case_index + 1
                    ))
                })?;
                if argv.is_empty() || argv.iter().any(|argument| is_shell_operator(argument)) {
                    return Err(ManifestError::new(format!(
                        "case {} verify command uses shell syntax without verify_command_mode = \"shell\": {command}",
                        case_index + 1
                    )));
                }
            }
            Ok(command)
        })
        .collect()
}

fn is_shell_operator(argument: &str) -> bool {
    matches!(argument, "&&" | "||" | ";" | "|" | "&")
        || argument.starts_with('>')
        || argument.starts_with('<')
        || argument.contains("2>")
}

pub(super) fn normalize_paths(
    paths: Vec<String>,
    case_index: usize,
) -> Result<Vec<String>, ManifestError> {
    paths
        .into_iter()
        .map(|path| {
            let path = non_empty(path, "path")?;
            validate_repo_relative_path(&path)
                .map_err(|error| ManifestError::new(format!("case {} {error}", case_index + 1)))?;
            Ok(path)
        })
        .collect()
}

pub(super) fn normalize_evidence(
    evidence: Vec<String>,
    case_index: usize,
) -> Result<Vec<String>, ManifestError> {
    evidence
        .into_iter()
        .map(|evidence| {
            let evidence = non_empty(evidence, "evidence")?;
            validate_single_line(&evidence, "evidence")
                .map_err(|error| ManifestError::new(format!("case {} {error}", case_index + 1)))?;
            Ok(evidence)
        })
        .collect()
}

pub(super) fn normalize_resolution_prs(
    resolution_prs: Vec<u64>,
    case_index: usize,
) -> Result<Vec<u64>, ManifestError> {
    if resolution_prs.contains(&0) {
        return Err(ManifestError::new(format!(
            "case {} resolution_prs must be greater than zero",
            case_index + 1
        )));
    }
    Ok(resolution_prs)
}

pub(super) fn normalize_resolution_commits(
    resolution_commits: Vec<String>,
    case_index: usize,
) -> Result<Vec<String>, ManifestError> {
    resolution_commits
        .into_iter()
        .map(|commit| {
            let commit = non_empty(commit, "resolution_commit")?;
            validate_base_commit(&commit)
                .map_err(|error| ManifestError::new(format!("case {} {error}", case_index + 1)))?;
            Ok(commit)
        })
        .collect()
}

pub(super) fn validate_resolution_metadata(
    commit_resolution: Option<EvalCommitResolution>,
    verdict: Option<EvalCaseVerdict>,
    resolution_prs: &[u64],
    resolution_commits: &[String],
    case_index: usize,
) -> Result<(), ManifestError> {
    if (!resolution_prs.is_empty() || !resolution_commits.is_empty()) && commit_resolution.is_none()
    {
        return Err(ManifestError::new(format!(
            "case {} commit_resolution is required when resolution metadata is present",
            case_index + 1
        )));
    }

    match commit_resolution {
        Some(EvalCommitResolution::Resolved) if resolution_commits.is_empty() => {
            Err(ManifestError::new(format!(
                "case {} resolved commit_resolution requires resolution_commits",
                case_index + 1
            )))
        }
        Some(EvalCommitResolution::Pending) if !resolution_commits.is_empty() => {
            Err(ManifestError::new(format!(
                "case {} pending commit_resolution must not include resolution_commits",
                case_index + 1
            )))
        }
        Some(EvalCommitResolution::Pending) if verdict == Some(EvalCaseVerdict::Replayable) => {
            Err(ManifestError::new(format!(
                "case {} pending commit_resolution cannot be replayable",
                case_index + 1
            )))
        }
        None if verdict == Some(EvalCaseVerdict::Replayable) => Err(ManifestError::new(format!(
            "case {} replayable verdict requires resolved commit_resolution",
            case_index + 1
        ))),
        _ => Ok(()),
    }
}

pub(super) fn validate_command_structure(command: &str) -> Result<(), ManifestError> {
    validate_single_line(command, "verify command")?;
    let Some(program) = command.split_whitespace().next() else {
        return Err(ManifestError::new("verify command must include a program"));
    };
    if program.starts_with('-') {
        return Err(ManifestError::new(format!(
            "verify command program must not start with '-': {command}"
        )));
    }
    Ok(())
}

pub(super) fn validate_repo_relative_path(path: &str) -> Result<(), ManifestError> {
    if path.starts_with('/') || path.starts_with('~') || path.contains('\\') {
        return Err(ManifestError::new(format!(
            "path must be repository-relative: {path}"
        )));
    }
    if path
        .split('/')
        .any(|segment| segment.is_empty() || segment == "." || segment == "..")
    {
        return Err(ManifestError::new(format!(
            "path must not contain empty, current, or parent segments: {path}"
        )));
    }
    Ok(())
}

pub(super) fn validate_single_line(value: &str, field: &str) -> Result<(), ManifestError> {
    if value.chars().any(|ch| matches!(ch, '\n' | '\r' | '\0')) {
        return Err(ManifestError::new(format!("{field} must be a single line")));
    }
    Ok(())
}

pub(super) fn validate_timeout(timeout_secs: u64, field: &str) -> Result<(), ManifestError> {
    if timeout_secs == 0 {
        return Err(ManifestError::new(format!(
            "{field} must be greater than zero"
        )));
    }
    Ok(())
}

pub(super) fn normalize_isolation_profile(
    mut profile: EvalIsolationProfile,
    context: &str,
) -> Result<EvalIsolationProfile, ManifestError> {
    profile.runtime_profile = non_empty(profile.runtime_profile, "eval isolation runtime_profile")?;
    profile.sandbox = non_empty(profile.sandbox, "eval isolation sandbox")?;
    profile.backend = non_empty(profile.backend, "eval isolation backend")?;
    profile.image = non_empty(profile.image, "eval isolation image")?;

    match profile.tier {
        IsolationTier::Host => {
            return Err(ManifestError::new(format!(
                "{context} tier must be container; host is not valid for untrusted eval cases"
            )));
        }
        IsolationTier::Container => {}
        IsolationTier::Microvm => {
            return Err(ManifestError::new(format!(
                "{context} tier `microvm` is reserved but not implemented; use container"
            )));
        }
    }
    if profile.runtime_kind != RuntimeKind::RemoteHost {
        return Err(ManifestError::new(format!(
            "{context} runtime_kind must be remote_host so eval cases cannot run in the caller or server process"
        )));
    }
    if profile.sandbox != DEFAULT_EVAL_ISOLATION_SANDBOX {
        return Err(ManifestError::new(format!(
            "{context} sandbox must be {DEFAULT_EVAL_ISOLATION_SANDBOX}"
        )));
    }
    if !profile.cleanup_required {
        return Err(ManifestError::new(format!(
            "{context} cleanup_required must be true"
        )));
    }

    Ok(profile)
}

pub(super) fn default_eval_isolation_tier() -> IsolationTier {
    IsolationTier::Container
}

pub(super) fn default_eval_isolation_runtime_kind() -> RuntimeKind {
    RuntimeKind::RemoteHost
}

pub(super) fn default_eval_isolation_runtime_profile() -> String {
    DEFAULT_EVAL_ISOLATION_RUNTIME_PROFILE.to_string()
}

pub(super) fn default_eval_isolation_sandbox() -> String {
    DEFAULT_EVAL_ISOLATION_SANDBOX.to_string()
}

pub(super) fn default_eval_isolation_backend() -> String {
    DEFAULT_EVAL_ISOLATION_BACKEND.to_string()
}

pub(super) fn default_eval_isolation_image() -> String {
    DEFAULT_EVAL_ISOLATION_IMAGE.to_string()
}

pub(super) fn default_cleanup_required() -> bool {
    true
}
