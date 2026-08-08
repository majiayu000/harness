use super::egress::{EgressPolicy, EgressProxyRoute};
use super::{prepare_agent_spawn, AgentSpawnInput};
use anyhow::Context;
use harness_core::agent::{AGENT_ISOLATION_TIER_ENV, AGENT_NETWORK_ALLOWLIST_ENV};
use harness_core::config::agents::AgentPermissionMode;
use harness_core::config::agents::SandboxMode;
use harness_core::config::isolation::IsolationTier;
use harness_sandbox::SandboxSpec;
use std::collections::HashMap;
use std::ffi::OsString;
use std::path::Path;
use std::process::Command;

#[test]
fn scoped_without_allowlist_denies_network() {
    assert_eq!(
        EgressPolicy::resolve(AgentPermissionMode::Scoped, &[]),
        EgressPolicy::Deny
    );
}

#[test]
fn explicit_full_without_allowlist_is_unrestricted() {
    assert_eq!(
        EgressPolicy::resolve(AgentPermissionMode::Full, &[]),
        EgressPolicy::Unrestricted
    );
}

#[test]
fn allowlist_always_requires_the_first_party_proxy() {
    let allowlist = ["github.com".to_string()];
    assert_eq!(
        EgressPolicy::resolve(AgentPermissionMode::Scoped, &allowlist),
        EgressPolicy::Proxy
    );
    assert_eq!(
        EgressPolicy::resolve(AgentPermissionMode::Full, &allowlist),
        EgressPolicy::Proxy
    );
}

#[test]
fn container_proxy_route_uses_an_isolated_network_and_canary_wrapper() {
    let route = EgressProxyRoute::container(
        "harness-egress-ar-123".to_string(),
        "http://egress-proxy:8080".to_string(),
    );

    assert_eq!(route.container_network(), Some("harness-egress-ar-123"));
    assert_eq!(route.proxy_url(), "http://egress-proxy:8080");
    assert!(route.requires_container_canary());
}

#[test]
fn host_proxy_route_exposes_only_a_local_port() {
    let route = EgressProxyRoute::host(18_080);

    assert_eq!(route.proxy_url(), "http://127.0.0.1:18080");
    assert_eq!(route.local_proxy_port(), Some(18_080));
    assert_eq!(route.container_network(), None);
    assert!(!route.requires_container_canary());
}

fn docker_test_env() -> HashMap<String, String> {
    let image = match harness_core::config::process_env::var("HARNESS_EGRESS_TEST_PROXY_IMAGE") {
        Ok(image) => image,
        Err(_) => "harness-egress-proxy:gh1771".to_string(),
    };
    HashMap::from([(
        harness_core::agent::AGENT_EGRESS_PROXY_IMAGE_ENV.to_string(),
        image,
    )])
}

#[test]
#[ignore = "requires Docker and the first-party proxy fixture image"]
fn docker_host_proxy_enforces_allowlist() -> anyhow::Result<()> {
    let lease = super::egress::EgressProxyLease::start(
        IsolationTier::Host,
        &["example.com".to_string()],
        &docker_test_env(),
    )?;

    let output = Command::new("curl")
        .args([
            "--silent",
            "--noproxy",
            "",
            "--proxy",
            lease.route().proxy_url(),
            "--output",
            "/dev/null",
            "--write-out",
            "%{http_code}",
            "--max-time",
            "10",
            "https://example.com/",
        ])
        .output()?;

    assert!(output.status.success());
    assert_eq!(String::from_utf8_lossy(&output.stdout), "200");
    Ok(())
}

#[test]
#[ignore = "requires Docker, Ubuntu fixture image, and the proxy fixture image"]
fn docker_container_network_forces_all_egress_through_proxy() -> anyhow::Result<()> {
    let lease = super::egress::EgressProxyLease::start(
        IsolationTier::Container,
        &["example.com".to_string()],
        &docker_test_env(),
    )?;
    let network = lease
        .route()
        .container_network()
        .context("container route should own an internal network")?;

    let allowed = Command::new("docker")
        .args([
            "run",
            "--rm",
            "--network",
            network,
            "ubuntu:24.04",
            "bash",
            "-lc",
            "exec 3<>/dev/tcp/egress-proxy/8080; printf 'GET http://example.com/ HTTP/1.1\\r\\nHost: example.com\\r\\nConnection: close\\r\\n\\r\\n' >&3; IFS= read -r status <&3; case \"$status\" in 'HTTP/1.1 200'*|'HTTP/1.1 301'*) exit 0;; *) echo \"$status\" >&2; exit 1;; esac",
        ])
        .status()?;
    assert!(allowed.success());

    let denied = Command::new("docker")
        .args([
            "run",
            "--rm",
            "--network",
            network,
            "ubuntu:24.04",
            "bash",
            "-lc",
            "exec 3<>/dev/tcp/egress-proxy/8080; printf 'GET http://denied.invalid/ HTTP/1.1\\r\\nHost: denied.invalid\\r\\nConnection: close\\r\\n\\r\\n' >&3; IFS= read -r status <&3; case \"$status\" in 'HTTP/1.1 403'*) exit 0;; *) echo \"$status\" >&2; exit 1;; esac",
        ])
        .status()?;
    assert!(denied.success());

    let bypass = Command::new("docker")
        .args([
            "run",
            "--rm",
            "--network",
            network,
            "ubuntu:24.04",
            "timeout",
            "2",
            "bash",
            "-lc",
            "exec 3<>/dev/tcp/example.com/80",
        ])
        .status()?;
    assert!(!bypass.success(), "internal network allowed direct egress");
    Ok(())
}

#[test]
#[ignore = "requires Docker plus the reference agent and proxy fixture images"]
fn docker_prepared_spawn_runs_canary_and_agent_behind_proxy() -> anyhow::Result<()> {
    let root = tempfile::tempdir()?;
    let mut env_vars = docker_test_env();
    env_vars.insert(
        AGENT_ISOLATION_TIER_ENV.to_string(),
        "container".to_string(),
    );
    env_vars.insert(
        AGENT_NETWORK_ALLOWLIST_ENV.to_string(),
        "example.com".to_string(),
    );
    env_vars.insert(
        "HARNESS_AGENT_CONTAINER_IMAGE".to_string(),
        "harness-agent:gh1771".to_string(),
    );
    let child_args = [
        OsString::from("-c"),
        OsString::from(
            r#"allowed="$(curl --silent --noproxy '' --proxy "$HTTP_PROXY" --output /dev/null --write-out '%{http_code}' --max-time 10 https://example.com/)"
test "$allowed" = 200
denied="$(curl --silent --noproxy '' --proxy "$HTTP_PROXY" --output /dev/null --write-out '%{http_code}' --max-time 5 http://denied.invalid/)"
test "$denied" = 403
if curl --silent --noproxy '*' --max-time 2 https://example.com/ >/dev/null 2>&1; then exit 71; fi"#,
        ),
    ];
    let sandbox_spec = SandboxSpec::new(SandboxMode::DangerFullAccess, root.path());
    let spawn = prepare_agent_spawn(AgentSpawnInput {
        program: Path::new("sh"),
        args: &child_args,
        project_root: root.path(),
        sandbox_spec: &sandbox_spec,
        env_vars: &env_vars,
        permission_mode: AgentPermissionMode::Scoped,
        forward_stdin: false,
    })?;

    let mut command = Command::new(&spawn.program);
    command
        .args(&spawn.args)
        .current_dir(&spawn.current_dir)
        .env_clear()
        .envs(&spawn.process_env);
    let output = command.output()?;

    assert!(
        output.status.success(),
        "prepared agent spawn failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}
