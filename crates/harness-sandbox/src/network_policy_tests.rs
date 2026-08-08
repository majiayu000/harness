use super::*;

#[test]
fn danger_mode_with_denied_network_is_not_a_passthrough() {
    let spec = SandboxSpec::new(SandboxMode::DangerFullAccess, "/tmp/project")
        .with_network_policy(NetworkPolicy::Deny);

    let wrapped = wrap_command(Path::new("/usr/bin/env"), &[], &spec).unwrap();

    assert_ne!(wrapped.engine, SandboxEngine::None);
    let rendered = wrapped
        .args
        .iter()
        .map(|arg| arg.to_string_lossy())
        .collect::<Vec<_>>()
        .join(" ");
    assert!(rendered.contains("(allow default)"));
    assert!(rendered.contains("(deny network-outbound)"));
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
