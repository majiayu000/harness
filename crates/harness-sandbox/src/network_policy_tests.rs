use super::*;

#[test]
fn danger_mode_with_denied_network_is_not_a_passthrough() {
    let spec = SandboxSpec::new(SandboxMode::DangerFullAccess, "/tmp/project")
        .with_network_policy(NetworkPolicy::Deny);

    let policy = seatbelt_policy(&spec).unwrap();

    assert!(policy.contains("(allow default)"));
    assert!(policy.contains("(deny network-outbound)"));
    assert!(policy.contains("(deny network-inbound)"));
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
    assert!(policy.contains("(deny network-inbound)"));
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

#[test]
fn eval_network_policy_defaults_to_deny_inbound_and_outbound() {
    let policy = EvalNetworkPolicy::for_allowlist(&[]).unwrap();

    assert_eq!(policy.inbound, EvalNetworkAccess::Deny);
    assert_eq!(policy.outbound, EvalNetworkAccess::Deny);
    assert!(policy.network_allowlist.is_empty());

    let report = EvalNetworkPolicyReport {
        runtime_job_id: "job-1".to_string(),
        enforced: true,
        policy: policy.clone(),
        grants: Vec::new(),
        connections: vec![NetworkConnectionMetadata {
            direction: NetworkDirection::Outbound,
            host: Some("denied.invalid".to_string()),
            port: Some(443),
            protocol: Some(NetworkProtocol::Tcp),
            decision: NetworkDecision::Denied,
            reason: "empty eval allowlist denies outbound connections".to_string(),
            bytes_sent: None,
            bytes_received: None,
        }],
        payloads_recorded: false,
        reason: "container network policy denied all eval networking".to_string(),
    };

    report.validate_against(&policy, "job-1").unwrap();
}

#[test]
fn eval_network_policy_allows_trusted_dns_allowlist_grants_without_payloads() {
    let policy = EvalNetworkPolicy::for_allowlist(&[
        " GitHub.COM. ".to_string(),
        "api.github.com".to_string(),
    ])
    .unwrap();

    assert_eq!(
        policy.network_allowlist,
        vec!["github.com".to_string(), "api.github.com".to_string()]
    );
    assert_eq!(policy.inbound, EvalNetworkAccess::Deny);
    assert_eq!(policy.outbound, EvalNetworkAccess::Allowlist);

    let report = EvalNetworkPolicyReport {
        runtime_job_id: "job-2".to_string(),
        enforced: true,
        policy: policy.clone(),
        grants: vec![
            NetworkPolicyGrant {
                direction: NetworkDirection::Outbound,
                host: "github.com".to_string(),
                port: Some(443),
                protocol: Some(NetworkProtocol::Https),
            },
            NetworkPolicyGrant {
                direction: NetworkDirection::Outbound,
                host: "api.github.com".to_string(),
                port: Some(443),
                protocol: Some(NetworkProtocol::Https),
            },
        ],
        connections: vec![NetworkConnectionMetadata {
            direction: NetworkDirection::Outbound,
            host: Some("github.com".to_string()),
            port: Some(443),
            protocol: Some(NetworkProtocol::Https),
            decision: NetworkDecision::Allowed,
            reason: "host matched trusted eval network allowlist".to_string(),
            bytes_sent: Some(128),
            bytes_received: Some(256),
        }],
        payloads_recorded: false,
        reason: "recorded grants and connection metadata only".to_string(),
    };

    report.validate_against(&policy, "job-2").unwrap();
}

#[test]
fn eval_network_policy_rejects_ambiguous_dns_allowlist_entries() {
    for host in [
        "https://github.com",
        "*.github.com",
        "127.0.0.1",
        "127.1",
        "127.0.0.01",
        "0177.0.0.1",
        "0x7f.0.0.1",
        "0x7f.1",
        "0X7F.0.0.1",
        "localhost",
        "github.com:443",
        "github.com/path",
    ] {
        let error = EvalNetworkPolicy::for_allowlist(&[host.to_string()])
            .expect_err("ambiguous eval allowlist host should fail closed");

        assert!(matches!(
            error,
            NetworkPolicyReportError::InvalidAllowlistHost { .. }
        ));
    }
}

#[test]
fn eval_network_policy_revalidated_rejects_injected_numeric_aliases() {
    let injected = EvalNetworkPolicy {
        inbound: EvalNetworkAccess::Allowlist,
        outbound: EvalNetworkAccess::Allowlist,
        network_allowlist: vec!["0x7f.0.0.1".to_string()],
    };
    let error = injected
        .revalidated()
        .expect_err("claim-time alias injection must fail closed");
    assert!(matches!(
        error,
        NetworkPolicyReportError::InvalidAllowlistHost { .. }
    ));
}

#[test]
fn eval_network_policy_revalidated_rebuilds_secure_shape() {
    let injected = EvalNetworkPolicy {
        inbound: EvalNetworkAccess::Allowlist,
        outbound: EvalNetworkAccess::Deny,
        network_allowlist: vec![" API.GitHub.COM. ".to_string()],
    };
    let policy = injected.revalidated().unwrap();
    assert_eq!(policy.inbound, EvalNetworkAccess::Deny);
    assert_eq!(policy.outbound, EvalNetworkAccess::Allowlist);
    assert_eq!(policy.network_allowlist, vec!["api.github.com".to_string()]);
}

#[test]
fn eval_network_policy_report_rejects_unsupported_allowed_connections() {
    let policy = EvalNetworkPolicy::for_allowlist(&[]).unwrap();
    let report = EvalNetworkPolicyReport {
        runtime_job_id: "job-3".to_string(),
        enforced: true,
        policy: policy.clone(),
        grants: Vec::new(),
        connections: vec![NetworkConnectionMetadata {
            direction: NetworkDirection::Outbound,
            host: Some("github.com".to_string()),
            port: Some(443),
            protocol: Some(NetworkProtocol::Https),
            decision: NetworkDecision::Allowed,
            reason: "backend reported an outbound connection despite deny-all policy".to_string(),
            bytes_sent: None,
            bytes_received: None,
        }],
        payloads_recorded: false,
        reason: "backend reported enforcement".to_string(),
    };

    assert_eq!(
        report.validate_against(&policy, "job-3"),
        Err(NetworkPolicyReportError::UnexpectedAllowedConnection)
    );
}

#[test]
fn eval_network_policy_inbound_allowlist_validates_host_fail_closed() {
    let policy = EvalNetworkPolicy {
        inbound: EvalNetworkAccess::Allowlist,
        outbound: EvalNetworkAccess::Deny,
        network_allowlist: vec!["api.github.com".to_string()],
    };
    let allowed = EvalNetworkPolicyReport {
        runtime_job_id: "job-in".to_string(),
        enforced: true,
        policy: policy.clone(),
        grants: vec![NetworkPolicyGrant {
            direction: NetworkDirection::Outbound,
            host: "api.github.com".to_string(),
            port: Some(443),
            protocol: Some(NetworkProtocol::Https),
        }],
        connections: vec![NetworkConnectionMetadata {
            direction: NetworkDirection::Inbound,
            host: Some("api.github.com".to_string()),
            port: Some(443),
            protocol: Some(NetworkProtocol::Https),
            decision: NetworkDecision::Allowed,
            reason: "inbound host matched allowlist".to_string(),
            bytes_sent: None,
            bytes_received: None,
        }],
        payloads_recorded: false,
        reason: "inbound allowlist validated".to_string(),
    };
    let rejected = EvalNetworkPolicyReport {
        runtime_job_id: "job-in".to_string(),
        enforced: true,
        policy: policy.clone(),
        grants: vec![NetworkPolicyGrant {
            direction: NetworkDirection::Outbound,
            host: "api.github.com".to_string(),
            port: Some(443),
            protocol: Some(NetworkProtocol::Https),
        }],
        connections: vec![NetworkConnectionMetadata {
            direction: NetworkDirection::Inbound,
            host: Some("evil.example".to_string()),
            port: Some(443),
            protocol: Some(NetworkProtocol::Https),
            decision: NetworkDecision::Allowed,
            reason: "inbound host outside allowlist".to_string(),
            bytes_sent: None,
            bytes_received: None,
        }],
        payloads_recorded: false,
        reason: "inbound allowlist validated".to_string(),
    };

    allowed.validate_against(&policy, "job-in").unwrap();
    assert_eq!(
        rejected.validate_against(&policy, "job-in"),
        Err(NetworkPolicyReportError::UnexpectedAllowedConnection)
    );
}

#[test]
fn eval_network_policy_rejects_unsupported_backend() {
    let policy = EvalNetworkPolicy::for_allowlist(&[]).unwrap();
    let err = validate_network_policy_backend(&policy, EvalNetworkPolicyBackend::Unsupported)
        .expect_err("unsupported backend must fail closed");
    assert!(matches!(
        err,
        NetworkPolicyReportError::UnsupportedBackend { .. }
    ));
}

#[test]
fn eval_network_policy_report_binds_runtime_job_id() {
    let policy = EvalNetworkPolicy::for_allowlist(&[]).unwrap();
    let report = EvalNetworkPolicyReport {
        runtime_job_id: "other-job".to_string(),
        enforced: true,
        policy: policy.clone(),
        grants: Vec::new(),
        connections: Vec::new(),
        payloads_recorded: false,
        reason: "enforced".to_string(),
    };
    assert_eq!(
        report.validate_against(&policy, "job-1"),
        Err(NetworkPolicyReportError::RuntimeJobMismatch)
    );
}
