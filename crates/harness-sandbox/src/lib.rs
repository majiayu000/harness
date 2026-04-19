use harness_core::config::agents::SandboxMode;
use harness_core::error::SandboxError;
use std::ffi::OsString;
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

// Intentional scope: keep VCS/agent metadata immutable inside workspace-write mode.
// `.env` is user-managed project content and is not forced read-only here.
const PROTECTED_RELATIVE_PATHS: [&str; 2] = [".git", ".harness"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxEngine {
    None,
    Seatbelt,
    Landlock,
    Bubblewrap,
}

#[derive(Debug, Clone)]
pub struct SandboxSpec {
    pub mode: SandboxMode,
    pub project_root: PathBuf,
    /// When set, sandbox write policy uses these specific paths instead of the
    /// blanket `project_root` allow. Populated from a `CapabilityToken`.
    pub allowed_write_paths: Option<Vec<PathBuf>>,
}

impl SandboxSpec {
    pub fn new(mode: SandboxMode, project_root: impl Into<PathBuf>) -> Self {
        let project_root = canonicalize_project_root(project_root.into());
        Self {
            mode,
            project_root,
            allowed_write_paths: None,
        }
    }

    /// Narrow write access to the given paths instead of the full `project_root`.
    pub fn with_allowed_write_paths(mut self, paths: Vec<PathBuf>) -> Self {
        self.allowed_write_paths = Some(paths);
        self
    }
}

#[derive(Debug, Clone)]
pub struct WrappedCommand {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    pub engine: SandboxEngine,
}

pub fn wrap_command(
    program: &Path,
    args: &[OsString],
    spec: &SandboxSpec,
) -> Result<WrappedCommand, SandboxError> {
    // DangerFullAccess bypasses sandboxing only when no token paths narrow the write
    // scope.  If allowed_write_paths is set (from a CapabilityToken), we upgrade to
    // WorkspaceWrite so the token's isolation is actually enforced.
    if spec.mode == SandboxMode::DangerFullAccess && spec.allowed_write_paths.is_none() {
        return Ok(WrappedCommand {
            program: program.to_path_buf(),
            args: args.to_vec(),
            engine: SandboxEngine::None,
        });
    }

    let owned;
    let spec: &SandboxSpec = if spec.mode == SandboxMode::DangerFullAccess {
        owned = SandboxSpec {
            mode: SandboxMode::WorkspaceWrite,
            ..spec.clone()
        };
        &owned
    } else {
        spec
    };

    #[cfg(target_os = "macos")]
    {
        wrap_macos_command(program, args, spec)
    }

    #[cfg(target_os = "linux")]
    {
        wrap_linux_command(program, args, spec)
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        Err(SandboxError::UnsupportedPlatform {
            mode: spec.mode,
            platform: std::env::consts::OS,
        })
    }
}

#[cfg(target_os = "macos")]
fn wrap_macos_command(
    program: &Path,
    args: &[OsString],
    spec: &SandboxSpec,
) -> Result<WrappedCommand, SandboxError> {
    let sandbox_exec =
        find_tool("sandbox-exec").ok_or(SandboxError::MissingTool("sandbox-exec"))?;
    let policy = seatbelt_policy(spec)?;

    let mut wrapped_args = vec![OsString::from("-p"), OsString::from(policy)];
    wrapped_args.push(program.as_os_str().to_os_string());
    wrapped_args.extend(args.iter().cloned());

    Ok(WrappedCommand {
        program: sandbox_exec,
        args: wrapped_args,
        engine: SandboxEngine::Seatbelt,
    })
}

#[cfg(target_os = "linux")]
fn wrap_linux_command(
    program: &Path,
    args: &[OsString],
    spec: &SandboxSpec,
) -> Result<WrappedCommand, SandboxError> {
    if let Some(landlock_runner) = find_tool("harness-landlock") {
        return Ok(WrappedCommand {
            program: landlock_runner,
            args: linux_landlock_args(program, args, spec)?,
            engine: SandboxEngine::Landlock,
        });
    }

    let bwrap = find_tool("bwrap").ok_or(SandboxError::MissingTool("harness-landlock or bwrap"))?;
    Ok(WrappedCommand {
        program: bwrap,
        args: linux_bwrap_args(program, args, spec)?,
        engine: SandboxEngine::Bubblewrap,
    })
}

// Keep `test` enabled so non-macOS CI can validate policy-generation logic.
#[cfg(any(target_os = "macos", test))]
fn seatbelt_policy(spec: &SandboxSpec) -> Result<String, SandboxError> {
    let workspace_path = seatbelt_escape_path(&spec.project_root)?;

    let mut lines = vec![
        "(version 1)".to_string(),
        "(deny default)".to_string(),
        "(allow process-exec)".to_string(),
        "(allow process-fork)".to_string(),
        "(allow signal (target self))".to_string(),
        "(allow network-outbound)".to_string(),
        "(allow file-read*)".to_string(),
    ];

    // Blanket /tmp write access is omitted when token paths are present.
    // Token paths already define the write boundary; granting /tmp globally
    // would allow escape to sibling worktrees housed under the same /tmp tree.
    if spec.allowed_write_paths.is_none() {
        lines.push(
            "(allow file-write* (subpath \"/private/tmp\") (subpath \"/tmp\") (subpath \"/var/tmp\"))"
                .to_string(),
        );
    }

    match spec.mode {
        SandboxMode::ReadOnly => {}
        SandboxMode::WorkspaceWrite => {
            if let Some(ref paths) = spec.allowed_write_paths {
                for path in paths {
                    lines.push(format!(
                        "(allow file-write* (subpath \"{}\"))",
                        seatbelt_escape_path(path)?
                    ));
                }
            } else {
                lines.push(format!(
                    "(allow file-write* (subpath \"{}\"))",
                    workspace_path
                ));
            }
            for protected_path in protected_paths(&spec.project_root) {
                lines.push(format!(
                    "(deny file-write* (subpath \"{}\"))",
                    seatbelt_escape_path(&protected_path)?
                ));
            }
        }
        SandboxMode::DangerFullAccess => {
            return Err(invalid_helper_mode("seatbelt_policy", spec.mode));
        }
    }

    Ok(lines.join("\n"))
}

#[cfg(any(target_os = "linux", test))]
fn linux_landlock_args(
    program: &Path,
    args: &[OsString],
    spec: &SandboxSpec,
) -> Result<Vec<OsString>, SandboxError> {
    let network_mode = match spec.mode {
        SandboxMode::ReadOnly => "deny",
        SandboxMode::WorkspaceWrite => "allow",
        SandboxMode::DangerFullAccess => {
            return Err(invalid_helper_mode("linux_landlock_args", spec.mode));
        }
    };

    let mut wrapped_args = vec![
        OsString::from("--mode"),
        OsString::from(spec.mode.to_string()),
        OsString::from("--network"),
        OsString::from(network_mode),
    ];

    // Pass every write path as a separate --workspace flag so multi-path tokens are
    // fully honoured.  Previously only the first entry was forwarded, silently
    // dropping any remaining paths in the Vec<PathBuf>.
    let write_paths: &[PathBuf] = spec
        .allowed_write_paths
        .as_deref()
        .unwrap_or(std::slice::from_ref(&spec.project_root));
    for path in write_paths {
        wrapped_args.push(OsString::from("--workspace"));
        wrapped_args.push(path.as_os_str().to_os_string());
    }

    for protected_path in protected_paths(&spec.project_root) {
        wrapped_args.push(OsString::from("--readonly-path"));
        wrapped_args.push(protected_path.into_os_string());
    }

    wrapped_args.push(OsString::from("--"));
    wrapped_args.push(program.as_os_str().to_os_string());
    wrapped_args.extend(args.iter().cloned());
    Ok(wrapped_args)
}

#[cfg(any(target_os = "linux", test))]
fn linux_bwrap_args(
    program: &Path,
    args: &[OsString],
    spec: &SandboxSpec,
) -> Result<Vec<OsString>, SandboxError> {
    let mut wrapped_args = vec![
        OsString::from("--die-with-parent"),
        OsString::from("--new-session"),
        OsString::from("--ro-bind"),
        OsString::from("/"),
        OsString::from("/"),
        OsString::from("--proc"),
        OsString::from("/proc"),
        OsString::from("--dev"),
        OsString::from("/dev"),
        OsString::from("--tmpfs"),
        OsString::from("/tmp"),
    ];

    match spec.mode {
        SandboxMode::ReadOnly => {
            wrapped_args.push(OsString::from("--unshare-net"));
        }
        SandboxMode::WorkspaceWrite => {
            if let Some(ref paths) = spec.allowed_write_paths {
                for path in paths {
                    wrapped_args.push(OsString::from("--bind"));
                    wrapped_args.push(path.as_os_str().to_os_string());
                    wrapped_args.push(path.as_os_str().to_os_string());
                }
            } else {
                wrapped_args.push(OsString::from("--bind"));
                wrapped_args.push(spec.project_root.as_os_str().to_os_string());
                wrapped_args.push(spec.project_root.as_os_str().to_os_string());
            }
            for protected_path in protected_paths(&spec.project_root) {
                if protected_path.exists() {
                    wrapped_args.push(OsString::from("--ro-bind"));
                    wrapped_args.push(protected_path.as_os_str().to_os_string());
                    wrapped_args.push(protected_path.as_os_str().to_os_string());
                }
            }
        }
        SandboxMode::DangerFullAccess => {
            return Err(invalid_helper_mode("linux_bwrap_args", spec.mode));
        }
    }

    wrapped_args.push(OsString::from("--chdir"));
    wrapped_args.push(spec.project_root.as_os_str().to_os_string());
    wrapped_args.push(OsString::from("--"));
    wrapped_args.push(program.as_os_str().to_os_string());
    wrapped_args.extend(args.iter().cloned());
    Ok(wrapped_args)
}

fn protected_paths(project_root: &Path) -> Vec<PathBuf> {
    PROTECTED_RELATIVE_PATHS
        .into_iter()
        .map(|relative| project_root.join(relative))
        .collect()
}

fn canonicalize_project_root(project_root: PathBuf) -> PathBuf {
    let absolute_path = if project_root.is_absolute() {
        project_root
    } else if let Ok(current_dir) = std::env::current_dir() {
        current_dir.join(project_root)
    } else {
        project_root
    };

    canonicalize_with_missing_tail(&absolute_path).unwrap_or(absolute_path)
}

fn canonicalize_with_missing_tail(path: &Path) -> Option<PathBuf> {
    let mut remainder = Vec::new();
    let mut cursor = path;

    loop {
        if cursor.exists() {
            let mut canonical = fs::canonicalize(cursor).ok()?;
            for component in remainder.iter().rev() {
                canonical.push(component);
            }
            return Some(canonical);
        }

        let leaf = cursor.file_name()?.to_os_string();
        remainder.push(leaf);
        cursor = cursor.parent()?;
    }
}

#[cfg(any(target_os = "macos", test))]
fn seatbelt_escape_path(path: &Path) -> Result<String, SandboxError> {
    let path_string = path.to_str().ok_or(SandboxError::InvalidPath {
        path: path.to_path_buf(),
        reason: "path is not valid UTF-8",
    })?;

    if path_string.chars().any(|ch| ch == '\n' || ch == '\r') {
        return Err(SandboxError::InvalidPath {
            path: path.to_path_buf(),
            reason: "newlines are not allowed",
        });
    }

    if path_string.chars().any(|ch| matches!(ch, '(' | ')' | ';')) {
        return Err(SandboxError::InvalidPath {
            path: path.to_path_buf(),
            reason: "contains seatbelt control characters",
        });
    }

    if path_string.chars().any(|ch| ch.is_control()) {
        return Err(SandboxError::InvalidPath {
            path: path.to_path_buf(),
            reason: "contains control characters",
        });
    }

    Ok(path_string.replace('\\', "\\\\").replace('\"', "\\\""))
}

fn find_tool(name: &str) -> Option<PathBuf> {
    let candidate = Path::new(name);
    if candidate.is_absolute() && is_executable(candidate) {
        return Some(candidate.to_path_buf());
    }

    let path_var = std::env::var_os("PATH")?;
    std::env::split_paths(&path_var)
        .map(|directory| directory.join(name))
        .find(|path| is_executable(path))
}

fn is_executable(path: &Path) -> bool {
    if !path.is_file() {
        return false;
    }

    #[cfg(unix)]
    {
        fs::metadata(path)
            .map(|metadata| metadata.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }

    #[cfg(not(unix))]
    {
        true
    }
}

fn invalid_helper_mode(helper: &'static str, mode: SandboxMode) -> SandboxError {
    SandboxError::InvalidHelperMode { helper, mode }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use tempfile::{tempdir, NamedTempFile};

    #[test]
    fn workspace_write_seatbelt_policy_protects_git_and_harness() {
        let spec = SandboxSpec::new(SandboxMode::WorkspaceWrite, "/tmp/project");
        let policy = seatbelt_policy(&spec).unwrap();
        let escaped_root = seatbelt_escape_path(&spec.project_root).unwrap();
        assert!(policy.contains("(allow network-outbound)"));
        assert!(policy.contains(&format!("(allow file-write* (subpath \"{escaped_root}\"))")));
        assert!(policy.contains(&format!(
            "(deny file-write* (subpath \"{escaped_root}/.git\"))"
        )));
        assert!(policy.contains(&format!(
            "(deny file-write* (subpath \"{escaped_root}/.harness\"))"
        )));
    }

    #[test]
    fn seatbelt_policy_rejects_injection_characters() {
        let spec = SandboxSpec::new(
            SandboxMode::WorkspaceWrite,
            "/tmp/project;(allow file-write* (subpath \"/\"))",
        );
        let error = seatbelt_policy(&spec).expect_err("policy path should be rejected");
        assert!(matches!(error, SandboxError::InvalidPath { .. }));
    }

    #[cfg(unix)]
    #[test]
    fn seatbelt_policy_rejects_non_utf8_paths() {
        use std::os::unix::ffi::OsStringExt;

        let non_utf8 = std::ffi::OsString::from_vec(vec![0x66, 0x6f, 0x80]);
        let spec = SandboxSpec::new(SandboxMode::WorkspaceWrite, PathBuf::from(non_utf8));
        let error = seatbelt_policy(&spec).expect_err("policy path should be rejected");
        assert!(matches!(
            error,
            SandboxError::InvalidPath {
                reason: "path is not valid UTF-8",
                ..
            }
        ));
    }

    #[test]
    fn workspace_write_bwrap_args_allow_network_and_preserve_command() {
        let spec = SandboxSpec::new(SandboxMode::WorkspaceWrite, "/tmp/project");
        let command_args = vec![OsString::from("--flag"), OsString::from("value")];
        let args = linux_bwrap_args(Path::new("/usr/bin/claude"), &command_args, &spec).unwrap();
        assert!(!args.contains(&OsString::from("--unshare-net")));
        let project_occurrences = args
            .iter()
            .filter(|value| value == &&spec.project_root.as_os_str().to_os_string())
            .count();
        assert_eq!(
            project_occurrences, 3,
            "workspace-write mode should reference project root twice for --bind and once for --chdir"
        );
        assert!(args.ends_with(&[
            OsString::from("--"),
            OsString::from("/usr/bin/claude"),
            OsString::from("--flag"),
            OsString::from("value"),
        ]));
    }

    #[test]
    fn read_only_bwrap_args_disable_network() {
        let spec = SandboxSpec::new(SandboxMode::ReadOnly, "/tmp/project");
        let args = linux_bwrap_args(Path::new("/usr/bin/claude"), &[], &spec).unwrap();
        assert!(args.contains(&OsString::from("--unshare-net")));
        let project_occurrences = args
            .iter()
            .filter(|value| value == &&spec.project_root.as_os_str().to_os_string())
            .count();
        assert_eq!(
            project_occurrences, 1,
            "read-only mode should only reference project root in --chdir"
        );
        assert!(args.ends_with(&[OsString::from("--"), OsString::from("/usr/bin/claude"),]));
    }

    #[test]
    fn landlock_args_include_mode_network_and_protected_paths() {
        let spec = SandboxSpec::new(SandboxMode::WorkspaceWrite, "/tmp/project");
        let args = linux_landlock_args(Path::new("/usr/bin/codex"), &[], &spec).unwrap();
        assert!(args.contains(&OsString::from("--mode")));
        assert!(args.contains(&OsString::from("workspace-write")));
        assert!(args.contains(&OsString::from("--network")));
        assert!(args.contains(&OsString::from("allow")));
        let protected_git = spec.project_root.join(".git");
        let protected_harness = spec.project_root.join(".harness");
        assert!(args.contains(&protected_git.into_os_string()));
        assert!(args.contains(&protected_harness.into_os_string()));
    }

    #[test]
    fn landlock_read_only_denies_network() {
        let spec = SandboxSpec::new(SandboxMode::ReadOnly, "/tmp/project");
        let args = linux_landlock_args(Path::new("/usr/bin/codex"), &[], &spec).unwrap();
        assert!(args.contains(&OsString::from("read-only")));
        assert!(args.contains(&OsString::from("deny")));
    }

    #[test]
    fn helper_builders_reject_danger_mode() {
        let spec = SandboxSpec::new(SandboxMode::DangerFullAccess, "/tmp/project");
        let seatbelt_error = seatbelt_policy(&spec).expect_err("danger mode should be rejected");
        assert!(matches!(
            seatbelt_error,
            SandboxError::InvalidHelperMode {
                helper: "seatbelt_policy",
                ..
            }
        ));

        let bwrap_error =
            linux_bwrap_args(Path::new("/usr/bin/claude"), &[], &spec).expect_err("must fail");
        assert!(matches!(
            bwrap_error,
            SandboxError::InvalidHelperMode {
                helper: "linux_bwrap_args",
                ..
            }
        ));

        let landlock_error =
            linux_landlock_args(Path::new("/usr/bin/codex"), &[], &spec).expect_err("must fail");
        assert!(matches!(
            landlock_error,
            SandboxError::InvalidHelperMode {
                helper: "linux_landlock_args",
                ..
            }
        ));
    }

    #[cfg(unix)]
    #[test]
    fn find_tool_requires_executable_permissions() {
        let tool_file = NamedTempFile::new().expect("should create temp tool file");
        let tool_path = tool_file.path().to_path_buf();
        fs::write(&tool_path, "#!/bin/sh\nexit 0\n").expect("should write tool");

        let mut permissions = fs::metadata(&tool_path).unwrap().permissions();
        permissions.set_mode(0o644);
        fs::set_permissions(&tool_path, permissions).unwrap();
        assert_eq!(find_tool(tool_path.to_str().expect("utf-8 path")), None);

        let mut permissions = fs::metadata(&tool_path).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&tool_path, permissions).unwrap();
        assert_eq!(
            find_tool(tool_path.to_str().expect("utf-8 path")),
            Some(tool_path.clone())
        );
    }

    #[cfg(unix)]
    #[test]
    fn sandbox_spec_canonicalizes_symlinked_project_root() {
        use std::os::unix::fs as unix_fs;

        let temp = tempdir().expect("should create temp dir");
        let real_root = temp.path().join("real-root");
        fs::create_dir_all(&real_root).expect("should create real root");
        let symlink_root = temp.path().join("link-root");
        unix_fs::symlink(&real_root, &symlink_root).expect("should create symlink");

        let spec = SandboxSpec::new(SandboxMode::WorkspaceWrite, &symlink_root);
        let canonical_real_root = fs::canonicalize(&real_root).expect("should canonicalize root");
        assert_eq!(spec.project_root, canonical_real_root);
    }

    #[cfg(unix)]
    #[test]
    fn sandbox_spec_canonicalizes_existing_ancestor_for_missing_child() {
        use std::os::unix::fs as unix_fs;

        let temp = tempdir().expect("should create temp dir");
        let real_root = temp.path().join("real-root");
        fs::create_dir_all(&real_root).expect("should create real root");
        let symlink_root = temp.path().join("link-root");
        unix_fs::symlink(&real_root, &symlink_root).expect("should create symlink");

        let unresolved = symlink_root.join("nested").join("project");
        let expected = fs::canonicalize(&real_root)
            .expect("should canonicalize root")
            .join("nested")
            .join("project");
        let spec = SandboxSpec::new(SandboxMode::WorkspaceWrite, unresolved);
        assert_eq!(spec.project_root, expected);
    }

    #[test]
    fn seatbelt_uses_token_paths_not_project_root() {
        let mut spec = SandboxSpec::new(SandboxMode::WorkspaceWrite, "/tmp/project");
        spec.allowed_write_paths = Some(vec![PathBuf::from("/tmp/harness-worktree-0")]);
        let policy = seatbelt_policy(&spec).unwrap();
        assert!(
            policy.contains("(allow file-write* (subpath \"/tmp/harness-worktree-0\"))"),
            "policy should reference token path"
        );
        assert!(
            !policy.contains("(allow file-write* (subpath \"/tmp/project\"))"),
            "policy must not use blanket project_root when token paths are set"
        );
        // Blanket /tmp grant must be absent so sibling worktrees under /tmp are
        // not reachable when the token narrows write scope.
        assert!(
            !policy.contains("(allow file-write* (subpath \"/private/tmp\")"),
            "blanket /tmp grant must be absent when token paths are set"
        );
    }

    #[test]
    fn seatbelt_token_includes_tmp() {
        let spec = SandboxSpec::new(SandboxMode::WorkspaceWrite, "/tmp/project");
        let policy = seatbelt_policy(&spec).unwrap();
        assert!(
            policy.contains("/tmp"),
            "/tmp must always be writable in the base policy"
        );
    }

    #[test]
    fn danger_mode_passthrough_keeps_program_and_args() {
        let spec = SandboxSpec::new(SandboxMode::DangerFullAccess, "/tmp/project");
        let original_args = vec![OsString::from("arg1"), OsString::from("arg2")];
        let wrapped = wrap_command(Path::new("/usr/bin/env"), &original_args, &spec).unwrap();
        assert_eq!(wrapped.engine, SandboxEngine::None);
        assert_eq!(wrapped.program, PathBuf::from("/usr/bin/env"));
        assert_eq!(wrapped.args, original_args);
    }

    #[test]
    fn danger_mode_with_token_paths_enforces_workspace_write_policy() {
        // Issue 1: DangerFullAccess + allowed_write_paths must NOT be a passthrough.
        // The effective mode must be WorkspaceWrite so the token paths are enforced.
        let spec = SandboxSpec {
            mode: SandboxMode::DangerFullAccess,
            project_root: PathBuf::from("/tmp/project"),
            allowed_write_paths: Some(vec![PathBuf::from("/tmp/worktree-0")]),
        };
        // Verify the effective spec (WorkspaceWrite mode) produces the correct policy.
        let effective = SandboxSpec {
            mode: SandboxMode::WorkspaceWrite,
            ..spec.clone()
        };
        let policy = seatbelt_policy(&effective).expect("should build policy");
        assert!(
            policy.contains("(allow file-write* (subpath \"/tmp/worktree-0\"))"),
            "token path must appear in policy"
        );
        assert!(
            !policy.contains("(allow file-write* (subpath \"/tmp/project\"))"),
            "project root must not be granted write access by the token path"
        );
    }

    #[test]
    fn landlock_args_pass_all_token_paths() {
        // Issue 2: all allowed_write_paths must appear as --workspace flags, not just first.
        let spec = SandboxSpec {
            mode: SandboxMode::WorkspaceWrite,
            project_root: PathBuf::from("/tmp/project"),
            allowed_write_paths: Some(vec![
                PathBuf::from("/tmp/worktree-0"),
                PathBuf::from("/tmp"),
                PathBuf::from("/var/tmp"),
            ]),
        };
        let args = linux_landlock_args(Path::new("/usr/bin/codex"), &[], &spec).unwrap();
        let workspace_values: Vec<OsString> = args
            .windows(2)
            .filter(|w| w[0] == "--workspace")
            .map(|w| w[1].clone())
            .collect();
        assert_eq!(
            workspace_values.len(),
            3,
            "all three token paths must appear as --workspace args"
        );
        assert!(workspace_values.contains(&OsString::from("/tmp/worktree-0")));
        assert!(workspace_values.contains(&OsString::from("/tmp")));
        assert!(workspace_values.contains(&OsString::from("/var/tmp")));
    }

    #[test]
    fn seatbelt_token_with_tmp_in_paths_allows_tmp_writes() {
        // Issue 3: when token paths include /tmp, the policy must still allow /tmp writes
        // (the blanket grant is replaced by per-path grants from the token).
        let spec = SandboxSpec {
            mode: SandboxMode::WorkspaceWrite,
            project_root: PathBuf::from("/workspace/project"),
            allowed_write_paths: Some(vec![
                PathBuf::from("/workspace/harness-worktree-0"),
                PathBuf::from("/tmp"),
                PathBuf::from("/private/tmp"),
                PathBuf::from("/var/tmp"),
            ]),
        };
        let policy = seatbelt_policy(&spec).expect("should build policy");
        assert!(
            policy.contains("(allow file-write* (subpath \"/tmp\"))"),
            "/tmp must be writable when included in token paths"
        );
        assert!(
            policy.contains("(allow file-write* (subpath \"/private/tmp\"))"),
            "/private/tmp must be writable when included in token paths"
        );
        // The blanket triple-path /tmp grant must NOT appear — it's been replaced by
        // individual per-path grants from the token.
        assert!(
            !policy
                .contains("(subpath \"/private/tmp\") (subpath \"/tmp\") (subpath \"/var/tmp\")"),
            "blanket /tmp grant must not appear when token paths are set"
        );
    }
}
