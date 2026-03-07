use harness_core::SandboxMode;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use thiserror::Error;

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
}

impl SandboxSpec {
    pub fn new(mode: SandboxMode, project_root: impl Into<PathBuf>) -> Self {
        Self {
            mode,
            project_root: project_root.into(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct WrappedCommand {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    pub engine: SandboxEngine,
}

#[derive(Debug, Error)]
pub enum SandboxError {
    #[error("sandbox mode `{mode}` is unsupported on `{platform}`")]
    UnsupportedPlatform {
        mode: SandboxMode,
        platform: &'static str,
    },
    #[error("sandbox tool not found: {0}")]
    MissingTool(&'static str),
}

pub fn wrap_command(
    program: &Path,
    args: &[OsString],
    spec: &SandboxSpec,
) -> Result<WrappedCommand, SandboxError> {
    if spec.mode == SandboxMode::DangerFullAccess {
        return Ok(WrappedCommand {
            program: program.to_path_buf(),
            args: args.to_vec(),
            engine: SandboxEngine::None,
        });
    }

    #[cfg(target_os = "macos")]
    {
        return wrap_macos_command(program, args, spec);
    }

    #[cfg(target_os = "linux")]
    {
        return wrap_linux_command(program, args, spec);
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
    let policy = seatbelt_policy(spec.mode, &spec.project_root);

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
            args: linux_landlock_args(program, args, spec),
            engine: SandboxEngine::Landlock,
        });
    }

    let bwrap = find_tool("bwrap").ok_or(SandboxError::MissingTool("harness-landlock or bwrap"))?;
    Ok(WrappedCommand {
        program: bwrap,
        args: linux_bwrap_args(program, args, spec),
        engine: SandboxEngine::Bubblewrap,
    })
}

#[allow(dead_code)]
fn seatbelt_policy(mode: SandboxMode, project_root: &Path) -> String {
    let mut lines = vec![
        "(version 1)".to_string(),
        "(deny default)".to_string(),
        "(allow process-exec)".to_string(),
        "(allow process-fork)".to_string(),
        "(allow signal (target self))".to_string(),
        "(allow file-read*)".to_string(),
        "(allow file-write* (subpath \"/private/tmp\") (subpath \"/tmp\") (subpath \"/var/tmp\"))"
            .to_string(),
    ];

    match mode {
        SandboxMode::ReadOnly => {}
        SandboxMode::WorkspaceWrite => {
            lines.push(format!(
                "(allow file-write* (subpath \"{}\"))",
                seatbelt_escape_path(project_root)
            ));
            for protected_path in protected_paths(project_root) {
                lines.push(format!(
                    "(deny file-write* (subpath \"{}\"))",
                    seatbelt_escape_path(&protected_path)
                ));
            }
        }
        SandboxMode::DangerFullAccess => {}
    }

    lines.join("\n")
}

#[allow(dead_code)]
fn linux_landlock_args(program: &Path, args: &[OsString], spec: &SandboxSpec) -> Vec<OsString> {
    let mut wrapped_args = vec![
        OsString::from("--mode"),
        OsString::from(sandbox_mode_cli(spec.mode)),
        OsString::from("--workspace"),
        spec.project_root.as_os_str().to_os_string(),
        OsString::from("--network"),
    ];

    if spec.mode == SandboxMode::WorkspaceWrite {
        wrapped_args.push(OsString::from("deny"));
    } else {
        wrapped_args.push(OsString::from("allow"));
    }

    for protected_path in protected_paths(&spec.project_root) {
        wrapped_args.push(OsString::from("--readonly-path"));
        wrapped_args.push(protected_path.into_os_string());
    }

    wrapped_args.push(OsString::from("--"));
    wrapped_args.push(program.as_os_str().to_os_string());
    wrapped_args.extend(args.iter().cloned());
    wrapped_args
}

#[allow(dead_code)]
fn linux_bwrap_args(program: &Path, args: &[OsString], spec: &SandboxSpec) -> Vec<OsString> {
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
            wrapped_args.push(OsString::from("--ro-bind"));
            wrapped_args.push(spec.project_root.as_os_str().to_os_string());
            wrapped_args.push(spec.project_root.as_os_str().to_os_string());
        }
        SandboxMode::WorkspaceWrite => {
            wrapped_args.push(OsString::from("--bind"));
            wrapped_args.push(spec.project_root.as_os_str().to_os_string());
            wrapped_args.push(spec.project_root.as_os_str().to_os_string());
            for protected_path in protected_paths(&spec.project_root) {
                if protected_path.exists() {
                    wrapped_args.push(OsString::from("--ro-bind"));
                    wrapped_args.push(protected_path.as_os_str().to_os_string());
                    wrapped_args.push(protected_path.as_os_str().to_os_string());
                }
            }
            wrapped_args.push(OsString::from("--unshare-net"));
        }
        SandboxMode::DangerFullAccess => {}
    }

    wrapped_args.push(OsString::from("--chdir"));
    wrapped_args.push(spec.project_root.as_os_str().to_os_string());
    wrapped_args.push(OsString::from("--"));
    wrapped_args.push(program.as_os_str().to_os_string());
    wrapped_args.extend(args.iter().cloned());
    wrapped_args
}

fn protected_paths(project_root: &Path) -> Vec<PathBuf> {
    PROTECTED_RELATIVE_PATHS
        .into_iter()
        .map(|relative| project_root.join(relative))
        .collect()
}

fn seatbelt_escape_path(path: &Path) -> String {
    path.to_string_lossy()
        .replace('\\', "\\\\")
        .replace('\"', "\\\"")
}

fn find_tool(name: &str) -> Option<PathBuf> {
    let candidate = Path::new(name);
    if candidate.is_absolute() && candidate.exists() {
        return Some(candidate.to_path_buf());
    }

    let path_var = std::env::var_os("PATH")?;
    std::env::split_paths(&path_var)
        .map(|directory| directory.join(name))
        .find(|path| path.is_file())
}

fn sandbox_mode_cli(mode: SandboxMode) -> &'static str {
    match mode {
        SandboxMode::ReadOnly => "read-only",
        SandboxMode::WorkspaceWrite => "workspace-write",
        SandboxMode::DangerFullAccess => "danger-full-access",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_write_seatbelt_policy_protects_git_and_harness() {
        let project_root = PathBuf::from("/tmp/project");
        let policy = seatbelt_policy(SandboxMode::WorkspaceWrite, &project_root);
        assert!(policy.contains("(allow file-write* (subpath \"/tmp/project\"))"));
        assert!(policy.contains("(deny file-write* (subpath \"/tmp/project/.git\"))"));
        assert!(policy.contains("(deny file-write* (subpath \"/tmp/project/.harness\"))"));
    }

    #[test]
    fn workspace_write_bwrap_args_disable_network_and_preserve_command() {
        let project_root = PathBuf::from("/tmp/project");
        let spec = SandboxSpec::new(SandboxMode::WorkspaceWrite, &project_root);
        let command_args = vec![OsString::from("--flag"), OsString::from("value")];
        let args = linux_bwrap_args(Path::new("/usr/bin/claude"), &command_args, &spec);
        assert!(args.contains(&OsString::from("--unshare-net")));
        assert!(args.ends_with(&[
            OsString::from("--"),
            OsString::from("/usr/bin/claude"),
            OsString::from("--flag"),
            OsString::from("value"),
        ]));
    }

    #[test]
    fn landlock_args_include_mode_network_and_protected_paths() {
        let project_root = PathBuf::from("/tmp/project");
        let spec = SandboxSpec::new(SandboxMode::WorkspaceWrite, &project_root);
        let args = linux_landlock_args(Path::new("/usr/bin/codex"), &[], &spec);
        assert!(args.contains(&OsString::from("--mode")));
        assert!(args.contains(&OsString::from("workspace-write")));
        assert!(args.contains(&OsString::from("--network")));
        assert!(args.contains(&OsString::from("deny")));
        assert!(args.contains(&OsString::from("/tmp/project/.git")));
        assert!(args.contains(&OsString::from("/tmp/project/.harness")));
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
}
