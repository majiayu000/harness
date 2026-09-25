use harness_core::error::HarnessError;
use std::ffi::OsString;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

const MANAGED_LABEL: &str = "com.harness.managed";
const MANAGED_VALUE: &str = "process-owned-v1";
const RESOURCE_LABEL: &str = "com.harness.resource";
const OWNER_PID_LABEL: &str = "com.harness.owner.pid";
const OWNER_TOKEN_LABEL: &str = "com.harness.owner.token";
static RESOURCE_SEQUENCE: AtomicU64 = AtomicU64::new(1);
static PROCESS_OWNER_TOKEN: OnceLock<String> = OnceLock::new();

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ManagedDockerResource {
    AgentContainer,
    EgressProxy,
    EgressNetwork,
}

impl ManagedDockerResource {
    fn label(self) -> &'static str {
        match self {
            Self::AgentContainer => "agent-container",
            Self::EgressProxy => "egress-proxy",
            Self::EgressNetwork => "egress-network",
        }
    }

    fn name_prefix(self) -> &'static str {
        match self {
            Self::AgentContainer => "harness-agent-",
            Self::EgressProxy => "harness-egress-proxy-",
            Self::EgressNetwork => "harness-egress-",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "agent-container" => Some(Self::AgentContainer),
            "egress-proxy" => Some(Self::EgressProxy),
            "egress-network" => Some(Self::EgressNetwork),
            _ => None,
        }
    }

    fn is_container(self) -> bool {
        matches!(self, Self::AgentContainer | Self::EgressProxy)
    }
}

pub(super) fn unique_resource_name(prefix: &str) -> String {
    format!("{prefix}{}", unique_suffix())
}

pub(super) fn append_string_labels(args: &mut Vec<String>, kind: ManagedDockerResource) {
    for (key, value) in ownership_labels(kind) {
        args.extend(["--label".to_string(), format!("{key}={value}")]);
    }
}

pub(super) fn append_os_labels(args: &mut Vec<OsString>, kind: ManagedDockerResource) {
    for (key, value) in ownership_labels(kind) {
        args.push(OsString::from("--label"));
        args.push(OsString::from(format!("{key}={value}")));
    }
}

pub(crate) fn reconcile_stale_resources() -> Result<(), HarnessError> {
    let current_pid = std::process::id();
    let current_token = process_owner_token();
    let mut failures = Vec::new();

    for resource in list_owned_resources(true)? {
        if resource.is_stale(current_pid, current_token) == Some(true) {
            if let Err(error) = remove_resource(&resource) {
                failures.push(error.to_string());
            }
        }
    }
    for resource in list_owned_resources(false)? {
        if resource.is_stale(current_pid, current_token) == Some(true) {
            if let Err(error) = remove_resource(&resource) {
                failures.push(error.to_string());
            }
        }
    }

    if failures.is_empty() {
        Ok(())
    } else {
        Err(agent_error(format!(
            "failed to reconcile stale Harness Docker resources: {}",
            failures.join("; ")
        )))
    }
}

#[derive(Debug, PartialEq, Eq)]
struct OwnedResource {
    name: String,
    kind: ManagedDockerResource,
    owner_pid: String,
    owner_token: String,
}

impl OwnedResource {
    fn is_stale(&self, current_pid: u32, current_token: &str) -> Option<bool> {
        owner_is_stale(
            &self.owner_pid,
            &self.owner_token,
            current_pid,
            current_token,
            process_is_alive,
        )
    }
}

fn list_owned_resources(containers: bool) -> Result<Vec<OwnedResource>, HarnessError> {
    let format = if containers {
        r#"{{.Names}}|{{.Label "com.harness.resource"}}|{{.Label "com.harness.owner.pid"}}|{{.Label "com.harness.owner.token"}}"#
    } else {
        r#"{{.Name}}|{{.Label "com.harness.resource"}}|{{.Label "com.harness.owner.pid"}}|{{.Label "com.harness.owner.token"}}"#
    };
    let args = if containers {
        vec![
            "ps",
            "--all",
            "--filter",
            "label=com.harness.managed=process-owned-v1",
            "--format",
            format,
        ]
    } else {
        vec![
            "network",
            "ls",
            "--filter",
            "label=com.harness.managed=process-owned-v1",
            "--format",
            format,
        ]
    };
    let output = run_docker(&args)?;
    let mut resources = Vec::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        match parse_resource(line, containers) {
            Some(resource) => resources.push(resource),
            None => tracing::error!(
                resource = line,
                "ignored malformed Harness Docker ownership record"
            ),
        }
    }
    Ok(resources)
}

fn parse_resource(line: &str, containers: bool) -> Option<OwnedResource> {
    let mut fields = line.splitn(4, '|');
    let name = fields.next()?.trim();
    let kind = ManagedDockerResource::parse(fields.next()?.trim())?;
    let owner_pid = fields.next()?.trim();
    let owner_token = fields.next()?.trim();
    if name.is_empty()
        || kind.is_container() != containers
        || !name.starts_with(kind.name_prefix())
        || owner_pid.is_empty()
        || owner_token.is_empty()
    {
        return None;
    }
    Some(OwnedResource {
        name: name.to_string(),
        kind,
        owner_pid: owner_pid.to_string(),
        owner_token: owner_token.to_string(),
    })
}

fn remove_resource(resource: &OwnedResource) -> Result<(), HarnessError> {
    let args = if resource.kind.is_container() {
        vec!["rm", "--force", resource.name.as_str()]
    } else {
        vec!["network", "rm", resource.name.as_str()]
    };
    let output = Command::new("docker")
        .args(&args)
        .output()
        .map_err(|error| agent_error(format!("failed to invoke Docker cleanup: {error}")))?;
    if output.status.success() {
        return Ok(());
    }
    let detail = docker_error_detail(&output);
    if detail.contains("No such container") || detail.contains("not found") {
        return Ok(());
    }
    Err(agent_error(format!(
        "failed to remove stale Docker {} `{}`: {detail}",
        resource.kind.label(),
        resource.name
    )))
}

fn owner_is_stale(
    owner_pid: &str,
    owner_token: &str,
    current_pid: u32,
    current_token: &str,
    is_alive: impl FnOnce(u32) -> bool,
) -> Option<bool> {
    let owner_pid = owner_pid.parse::<u32>().ok()?;
    if owner_token == current_token {
        return Some(false);
    }
    if owner_pid == current_pid {
        return Some(true);
    }
    Some(!is_alive(owner_pid))
}

#[cfg(unix)]
fn process_is_alive(pid: u32) -> bool {
    if pid == 0 || pid > i32::MAX as u32 {
        return false;
    }
    // SAFETY: signal 0 performs existence/permission checking only.
    let result = unsafe { unix_kill(pid as i32, 0) };
    result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(1)
}

#[cfg(unix)]
unsafe fn unix_kill(pid: i32, signal: i32) -> i32 {
    extern "C" {
        fn kill(pid: i32, signal: i32) -> i32;
    }
    kill(pid, signal)
}

#[cfg(not(unix))]
fn process_is_alive(_pid: u32) -> bool {
    // Preserve resources when this platform cannot safely probe another
    // process. Matching-current-PID stale resources are still reconciled.
    true
}

fn ownership_labels(kind: ManagedDockerResource) -> [(String, String); 4] {
    [
        (MANAGED_LABEL.to_string(), MANAGED_VALUE.to_string()),
        (RESOURCE_LABEL.to_string(), kind.label().to_string()),
        (OWNER_PID_LABEL.to_string(), std::process::id().to_string()),
        (
            OWNER_TOKEN_LABEL.to_string(),
            process_owner_token().to_string(),
        ),
    ]
}

fn process_owner_token() -> &'static str {
    PROCESS_OWNER_TOKEN.get_or_init(|| format!("{}-{}", std::process::id(), unique_suffix()))
}

fn unique_suffix() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let sequence = RESOURCE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    format!("{}-{nanos}-{sequence}", std::process::id())
}

fn run_docker(args: &[&str]) -> Result<Output, HarnessError> {
    let output = Command::new("docker")
        .args(args)
        .output()
        .map_err(|error| agent_error(format!("failed to invoke Docker reconciliation: {error}")))?;
    if output.status.success() {
        Ok(output)
    } else {
        Err(agent_error(format!(
            "Docker reconciliation probe failed: {}",
            docker_error_detail(&output)
        )))
    }
}

fn docker_error_detail(output: &Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    stderr
        .lines()
        .chain(stdout.lines())
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("Docker command failed")
        .to_string()
}

fn agent_error(message: impl Into<String>) -> HarnessError {
    HarnessError::AgentExecution(message.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_identify_the_resource_and_process_owner() {
        let labels = ownership_labels(ManagedDockerResource::EgressProxy);

        assert!(labels
            .iter()
            .any(|(key, value)| key == MANAGED_LABEL && value == MANAGED_VALUE));
        assert!(labels
            .iter()
            .any(|(key, value)| key == RESOURCE_LABEL && value == "egress-proxy"));
        assert!(labels.iter().any(|(key, value)| {
            key == OWNER_PID_LABEL && value == &std::process::id().to_string()
        }));
        assert!(labels
            .iter()
            .any(|(key, value)| key == OWNER_TOKEN_LABEL && value == process_owner_token()));
    }

    #[test]
    fn dead_owner_is_stale_but_live_owner_is_preserved() {
        assert_eq!(
            owner_is_stale("41", "old", 42, "current", |_| false),
            Some(true)
        );
        assert_eq!(
            owner_is_stale("41", "old", 42, "current", |_| true),
            Some(false)
        );
    }

    #[test]
    fn reused_current_pid_with_an_old_token_is_stale() {
        assert_eq!(
            owner_is_stale("42", "old", 42, "current", |_| true),
            Some(true)
        );
        assert_eq!(
            owner_is_stale("42", "current", 42, "current", |_| false),
            Some(false)
        );
    }

    #[test]
    fn malformed_or_unexpected_records_are_not_cleanup_targets() {
        assert!(parse_resource("user-container|egress-proxy|41|old", true).is_none());
        assert!(parse_resource("harness-egress-proxy-1|unknown|41|old", true).is_none());
        assert!(parse_resource("harness-egress-1|egress-network||old", false).is_none());
    }
}
