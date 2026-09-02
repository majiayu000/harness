use super::*;

#[test]
fn danger_mode_with_denied_network_is_not_a_passthrough() {
    let spec = SandboxSpec::new(SandboxMode::DangerFullAccess, "/tmp/project")
        .with_network_policy(NetworkPolicy::Deny);

    let policy = seatbelt_policy(&spec).unwrap();

    assert!(policy.contains("(allow default)"));
    assert!(policy.contains("(deny network-outbound)"));
}

#[test]
fn scoped_seatbelt_policy_denies_all_network_without_an_allowlist() {
    let spec = SandboxSpec::new(SandboxMode::WorkspaceWrite, "/tmp/project")
        .with_network_policy(NetworkPolicy::Deny);

    let policy = seatbelt_policy(&spec).unwrap();

    assert!(!policy.contains("(allow network-outbound)"));
}

#[test]
fn danger_seatbelt_policy_allows_only_the_local_proxy() {
    let spec = SandboxSpec::new(SandboxMode::DangerFullAccess, "/tmp/project")
        .with_network_policy(NetworkPolicy::LocalProxy { port: 18_080 });

    let policy = seatbelt_policy(&spec).unwrap();

    assert!(policy.contains("(allow default)"));
    assert!(
        policy.contains("(deny network-outbound (require-not (remote tcp \"localhost:18080\")))")
    );
    assert!(!policy.contains("(allow network-outbound)"));
}

#[test]
fn workspace_seatbelt_policy_allows_only_the_local_proxy() {
    let spec = SandboxSpec::new(SandboxMode::WorkspaceWrite, "/tmp/project")
        .with_network_policy(NetworkPolicy::LocalProxy { port: 18_080 });

    let policy = seatbelt_policy(&spec).unwrap();

    assert!(policy.contains("(allow network-outbound (remote tcp \"localhost:18080\"))"));
    assert!(!policy.contains("(allow network-outbound)"));
}

#[test]
fn linux_proxy_only_policy_fails_closed() {
    let spec = SandboxSpec::new(SandboxMode::WorkspaceWrite, "/tmp/project")
        .with_network_policy(NetworkPolicy::LocalProxy { port: 18_080 });

    let error = linux_bwrap_args(Path::new("/usr/bin/env"), &[], &spec)
        .expect_err("Linux host proxy-only enforcement must not silently allow full egress");

    assert!(matches!(
        error,
        SandboxError::UnsupportedNetworkPolicy { .. }
    ));
}

#[test]
fn danger_deny_requires_bwrap_even_when_landlock_is_available() {
    let spec = SandboxSpec::new(SandboxMode::DangerFullAccess, "/tmp/project")
        .with_network_policy(NetworkPolicy::Deny);
    let error = wrap_linux_command_with_tools(
        Path::new("/usr/bin/codex"),
        &[],
        &spec,
        Some(PathBuf::from("/usr/bin/harness-landlock")),
        None,
    )
    .expect_err("Landlock cannot provide network-only isolation");

    assert!(matches!(
        error,
        SandboxError::MissingTool(
            "bwrap (required for danger-full-access with deny-all networking)"
        )
    ));

    let wrapped = wrap_linux_command_with_tools(
        Path::new("/usr/bin/codex"),
        &[],
        &spec,
        Some(PathBuf::from("/usr/bin/harness-landlock")),
        Some(PathBuf::from("/usr/bin/bwrap")),
    )
    .expect("Bubblewrap should provide the network-only boundary");
    assert_eq!(wrapped.engine, SandboxEngine::Bubblewrap);
    assert!(wrapped.args.contains(&OsString::from("--unshare-net")));
}

#[cfg(target_os = "linux")]
#[test]
fn linux_network_only_bwrap_isolates_the_process_tree() {
    let spec = SandboxSpec::new(SandboxMode::DangerFullAccess, "/tmp/project")
        .with_network_policy(NetworkPolicy::Deny);

    let args = linux_network_only_bwrap_args(Path::new("/usr/bin/env"), &[], &spec);

    assert!(args.contains(&OsString::from("--unshare-pid")));
    assert!(args.contains(&OsString::from("--die-with-parent")));
}
