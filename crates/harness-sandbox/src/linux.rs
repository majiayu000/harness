use super::{invalid_helper_mode, protected_paths, NetworkPolicy, SandboxSpec};
use harness_core::config::agents::SandboxMode;
use harness_core::error::SandboxError;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

pub(super) fn linux_landlock_args(
    program: &Path,
    args: &[OsString],
    spec: &SandboxSpec,
) -> Result<Vec<OsString>, SandboxError> {
    if spec.network_policy.is_local_proxy() {
        return Err(spec.network_policy.unsupported("harness-landlock"));
    }
    let network_mode = match spec.mode {
        _ if spec.network_policy == NetworkPolicy::Deny => "deny",
        SandboxMode::ReadOnly => "deny",
        SandboxMode::ReadOnlyWithNetwork => "allow",
        SandboxMode::WorkspaceWrite => "allow",
        SandboxMode::DangerFullAccess => {
            return Err(invalid_helper_mode("linux_landlock_args", spec.mode));
        }
    };

    let mut wrapped_args = vec![
        OsString::from("--mode"),
        OsString::from(match spec.mode {
            SandboxMode::ReadOnlyWithNetwork => SandboxMode::ReadOnly.to_string(),
            mode => mode.to_string(),
        }),
        OsString::from("--network"),
        OsString::from(network_mode),
    ];

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

pub(super) fn linux_bwrap_args(
    program: &Path,
    args: &[OsString],
    spec: &SandboxSpec,
) -> Result<Vec<OsString>, SandboxError> {
    if spec.network_policy.is_local_proxy() {
        return Err(spec.network_policy.unsupported("bubblewrap"));
    }
    let mut wrapped_args = vec![
        OsString::from("--die-with-parent"),
        OsString::from("--new-session"),
        OsString::from("--unshare-pid"),
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

    if spec.network_policy == NetworkPolicy::Deny {
        wrapped_args.push(OsString::from("--unshare-net"));
    }

    match spec.mode {
        SandboxMode::ReadOnly => {
            if spec.network_policy != NetworkPolicy::Deny {
                wrapped_args.push(OsString::from("--unshare-net"));
            }
        }
        SandboxMode::ReadOnlyWithNetwork => {}
        SandboxMode::WorkspaceWrite => {
            if let Some(ref paths) = spec.allowed_write_paths {
                for path in paths {
                    if path == Path::new("/tmp") || !path.exists() {
                        continue;
                    }
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

#[cfg(any(target_os = "linux", test))]
pub(super) fn linux_network_only_bwrap_args(
    program: &Path,
    args: &[OsString],
    spec: &SandboxSpec,
) -> Vec<OsString> {
    let mut wrapped_args = vec![
        OsString::from("--die-with-parent"),
        OsString::from("--new-session"),
        OsString::from("--unshare-pid"),
        OsString::from("--bind"),
        OsString::from("/"),
        OsString::from("/"),
        OsString::from("--proc"),
        OsString::from("/proc"),
        OsString::from("--dev"),
        OsString::from("/dev"),
        OsString::from("--unshare-net"),
        OsString::from("--chdir"),
        spec.project_root.as_os_str().to_os_string(),
        OsString::from("--"),
        program.as_os_str().to_os_string(),
    ];
    wrapped_args.extend(args.iter().cloned());
    wrapped_args
}
