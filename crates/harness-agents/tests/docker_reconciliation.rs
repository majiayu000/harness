use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

struct DockerCleanup {
    containers: Vec<String>,
    network: String,
}

impl Drop for DockerCleanup {
    fn drop(&mut self) {
        for container in &self.containers {
            let _ = Command::new("docker")
                .args(["rm", "--force", container])
                .output();
        }
        let _ = Command::new("docker")
            .args(["network", "rm", &self.network])
            .output();
    }
}

fn docker(args: &[&str]) -> anyhow::Result<Output> {
    let output = Command::new("docker").args(args).output()?;
    anyhow::ensure!(
        output.status.success(),
        "docker command failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(output)
}

fn stale_labels(resource: &str) -> Vec<String> {
    vec![
        "--label".to_string(),
        "com.harness.managed=process-owned-v1".to_string(),
        "--label".to_string(),
        format!("com.harness.resource={resource}"),
        "--label".to_string(),
        "com.harness.owner.pid=4294967295".to_string(),
        "--label".to_string(),
        "com.harness.owner.token=stale-integration-owner".to_string(),
    ]
}

#[test]
#[ignore = "requires Docker and a local PostgreSQL image fixture"]
fn startup_reconciliation_removes_stale_containers_before_their_network() -> anyhow::Result<()> {
    docker(&["info"])?;
    let suffix = format!(
        "{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    );
    let network = format!("harness-egress-{suffix}");
    let proxy = format!("harness-egress-proxy-{suffix}");
    let agent = format!("harness-agent-{suffix}");
    let cleanup = DockerCleanup {
        containers: vec![proxy.clone(), agent.clone()],
        network: network.clone(),
    };

    let mut network_args = vec![
        "network".to_string(),
        "create".to_string(),
        "--internal".to_string(),
    ];
    network_args.extend(stale_labels("egress-network"));
    network_args.push(network.clone());
    docker(&network_args.iter().map(String::as_str).collect::<Vec<_>>())?;

    let image = harness_core::config::process_env::var("HARNESS_DOCKER_RECONCILIATION_TEST_IMAGE")
        .unwrap_or_else(|_| "postgres:16-alpine".to_string());
    for (name, resource) in [(&proxy, "egress-proxy"), (&agent, "agent-container")] {
        let mut args = vec![
            "run".to_string(),
            "--detach".to_string(),
            "--rm".to_string(),
            "--name".to_string(),
            name.clone(),
            "--network".to_string(),
            network.clone(),
        ];
        args.extend(stale_labels(resource));
        args.extend([
            "--entrypoint".to_string(),
            "sleep".to_string(),
            image.clone(),
            "300".to_string(),
        ]);
        docker(&args.iter().map(String::as_str).collect::<Vec<_>>())?;
    }

    harness_agents::docker_reconciliation::reconcile_stale_resources()?;

    assert!(!Command::new("docker")
        .args(["inspect", &proxy])
        .output()?
        .status
        .success());
    assert!(!Command::new("docker")
        .args(["inspect", &agent])
        .output()?
        .status
        .success());
    assert!(!Command::new("docker")
        .args(["network", "inspect", &network])
        .output()?
        .status
        .success());
    drop(cleanup);
    Ok(())
}
