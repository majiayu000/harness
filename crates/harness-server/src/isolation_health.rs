use harness_core::agent::AgentEgressMode;
use harness_core::config::isolation::{IsolationAvailability, IsolationTier, IsolationTierStatus};
use harness_core::config::HarnessConfig;
use harness_sandbox::{NetworkPolicy, SandboxSpec};
use tokio::{process::Command, time::Duration};

const DOCKER_PROBE_TIMEOUT: Duration = Duration::from_secs(2);

pub(crate) async fn probe_isolation_availability(config: &HarnessConfig) -> IsolationAvailability {
    availability_from_statuses(probe_host_tier(config), probe_container_tier().await)
}

#[cfg(test)]
pub(crate) fn availability_from_container_status(
    container_status: IsolationTierStatus,
) -> IsolationAvailability {
    availability_from_statuses(
        IsolationTierStatus::available(IsolationTier::Host),
        container_status,
    )
}

fn availability_from_statuses(
    host_status: IsolationTierStatus,
    container_status: IsolationTierStatus,
) -> IsolationAvailability {
    IsolationAvailability::new(vec![
        host_status,
        container_status,
        IsolationTierStatus::unavailable(
            IsolationTier::Microvm,
            "isolation tier `microvm` is reserved but not implemented",
        ),
    ])
}

fn probe_host_tier(config: &HarnessConfig) -> IsolationTierStatus {
    let network_policy = host_network_policy(config);
    let spec = SandboxSpec::new(config.agents.sandbox_mode, &config.server.project_root)
        .with_network_policy(network_policy);
    match harness_sandbox::validate_host_sandbox_support(&spec) {
        Ok(_) => IsolationTierStatus::available(IsolationTier::Host),
        Err(error) => IsolationTierStatus::unavailable(
            IsolationTier::Host,
            format!("host sandbox probe failed: {error}"),
        ),
    }
}

fn host_network_policy(config: &HarnessConfig) -> NetworkPolicy {
    let egress_mode = AgentEgressMode::resolve(
        config.agents.resolve_permission_mode(),
        &config.isolation.network_allowlist,
    );
    match egress_mode {
        AgentEgressMode::DenyAll => NetworkPolicy::Deny,
        AgentEgressMode::FirstPartyProxy => NetworkPolicy::LocalProxy { port: 1 },
        AgentEgressMode::Unrestricted => NetworkPolicy::InheritSandboxMode,
    }
}

async fn probe_container_tier() -> IsolationTierStatus {
    let probe = Command::new("docker")
        .arg("info")
        .arg("--format")
        .arg("{{.ServerVersion}}")
        .output();
    match tokio::time::timeout(DOCKER_PROBE_TIMEOUT, probe).await {
        Ok(Ok(output)) if output.status.success() => {
            IsolationTierStatus::available(IsolationTier::Container)
        }
        Ok(Ok(output)) => IsolationTierStatus::unavailable(
            IsolationTier::Container,
            docker_probe_failure_reason(&output),
        ),
        Ok(Err(error)) => IsolationTierStatus::unavailable(
            IsolationTier::Container,
            format!("docker CLI probe failed: {error}"),
        ),
        Err(_) => {
            IsolationTierStatus::unavailable(IsolationTier::Container, "docker CLI probe timed out")
        }
    }
}

fn docker_probe_failure_reason(output: &std::process::Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let detail = stderr
        .lines()
        .chain(stdout.lines())
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("docker info failed");
    format!("docker CLI probe failed: {detail}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn availability_marks_container_unavailable_from_probe_status() {
        let availability = availability_from_container_status(IsolationTierStatus::unavailable(
            IsolationTier::Container,
            "docker missing",
        ));

        let container = availability.status_for(IsolationTier::Container);
        let host = availability.status_for(IsolationTier::Host);

        assert!(host.available);
        assert!(!container.available);
        assert_eq!(container.reason.as_deref(), Some("docker missing"));
    }

    #[test]
    fn availability_preserves_host_probe_failure() {
        let availability = availability_from_statuses(
            IsolationTierStatus::unavailable(IsolationTier::Host, "bwrap missing"),
            IsolationTierStatus::available(IsolationTier::Container),
        );

        let host = availability.status_for(IsolationTier::Host);
        assert!(!host.available);
        assert_eq!(host.reason.as_deref(), Some("bwrap missing"));
    }

    #[test]
    fn scoped_empty_allowlist_requires_deny_all_host_networking() {
        let config = HarnessConfig::default();

        assert_eq!(host_network_policy(&config), NetworkPolicy::Deny);
    }
}
