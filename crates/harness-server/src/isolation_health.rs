use harness_core::config::isolation::{IsolationAvailability, IsolationTier, IsolationTierStatus};
use tokio::{process::Command, time::Duration};

const DOCKER_PROBE_TIMEOUT: Duration = Duration::from_secs(2);

pub(crate) async fn probe_isolation_availability() -> IsolationAvailability {
    availability_from_container_status(probe_container_tier().await)
}

pub(crate) fn availability_from_container_status(
    container_status: IsolationTierStatus,
) -> IsolationAvailability {
    IsolationAvailability::new(vec![
        IsolationTierStatus::available(IsolationTier::Host),
        container_status,
        IsolationTierStatus::unavailable(
            IsolationTier::Microvm,
            "isolation tier `microvm` is reserved but not implemented",
        ),
    ])
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
}
