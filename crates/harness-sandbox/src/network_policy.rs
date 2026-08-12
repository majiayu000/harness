#[cfg(any(target_os = "macos", test))]
use harness_core::config::agents::SandboxMode;
#[cfg(any(target_os = "linux", test))]
use harness_core::error::SandboxError;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use thiserror::Error;

pub const EVAL_NETWORK_POLICY_CAPABILITY: &str = "eval_network_policy";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum NetworkPolicy {
    #[default]
    InheritSandboxMode,
    Deny,
    LocalProxy {
        port: u16,
    },
}

impl NetworkPolicy {
    #[cfg(any(target_os = "linux", test))]
    pub(crate) fn is_local_proxy(self) -> bool {
        matches!(self, Self::LocalProxy { .. })
    }

    #[cfg(any(target_os = "linux", test))]
    pub(crate) fn unsupported(self, helper: &'static str) -> SandboxError {
        SandboxError::UnsupportedNetworkPolicy {
            helper,
            policy: self.as_str(),
        }
    }

    #[cfg(any(target_os = "linux", test))]
    fn as_str(self) -> &'static str {
        match self {
            Self::InheritSandboxMode => "inherit-sandbox-mode",
            Self::Deny => "deny",
            Self::LocalProxy { .. } => "local-proxy",
        }
    }

    #[cfg(any(target_os = "macos", test))]
    pub(crate) fn seatbelt_rules(self, mode: SandboxMode) -> Vec<String> {
        match (self, mode) {
            (Self::Deny, SandboxMode::DangerFullAccess) => {
                vec!["(deny network-outbound)".to_string()]
            }
            (Self::Deny, _) | (Self::InheritSandboxMode, SandboxMode::ReadOnly) => Vec::new(),
            (Self::LocalProxy { port }, SandboxMode::DangerFullAccess) => vec![format!(
                "(deny network-outbound (require-not (remote tcp \"localhost:{port}\")))"
            )],
            (Self::LocalProxy { port }, _) => vec![format!(
                "(allow network-outbound (remote tcp \"localhost:{port}\"))"
            )],
            (Self::InheritSandboxMode, _) => vec!["(allow network-outbound)".to_string()],
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvalNetworkPolicy {
    pub inbound: EvalNetworkAccess,
    pub outbound: EvalNetworkAccess,
    #[serde(default)]
    pub network_allowlist: Vec<String>,
}

impl EvalNetworkPolicy {
    pub fn for_allowlist(allowlist: &[String]) -> Result<Self, NetworkPolicyReportError> {
        let network_allowlist = normalize_dns_allowlist(allowlist)?;
        let outbound = if network_allowlist.is_empty() {
            EvalNetworkAccess::Deny
        } else {
            EvalNetworkAccess::Allowlist
        };
        Ok(Self {
            inbound: EvalNetworkAccess::Deny,
            outbound,
            network_allowlist,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvalNetworkAccess {
    Deny,
    Allowlist,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvalNetworkPolicyReport {
    pub enforced: bool,
    pub policy: EvalNetworkPolicy,
    #[serde(default)]
    pub grants: Vec<NetworkPolicyGrant>,
    #[serde(default)]
    pub connections: Vec<NetworkConnectionMetadata>,
    pub payloads_recorded: bool,
    pub reason: String,
}

impl EvalNetworkPolicyReport {
    pub fn validate_against(
        &self,
        expected: &EvalNetworkPolicy,
    ) -> Result<(), NetworkPolicyReportError> {
        if !self.enforced {
            return Err(NetworkPolicyReportError::EnforcementMissing);
        }
        if self.payloads_recorded {
            return Err(NetworkPolicyReportError::PayloadRecordingForbidden);
        }
        if self.reason.trim().is_empty() {
            return Err(NetworkPolicyReportError::MissingReason);
        }
        if self.policy != *expected {
            return Err(NetworkPolicyReportError::PolicyMismatch);
        }
        validate_grants(&self.grants, expected)?;
        for connection in &self.connections {
            validate_connection(connection, expected)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkPolicyGrant {
    pub direction: NetworkDirection,
    pub host: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol: Option<NetworkProtocol>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkConnectionMetadata {
    pub direction: NetworkDirection,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol: Option<NetworkProtocol>,
    pub decision: NetworkDecision,
    pub reason: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes_sent: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes_received: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NetworkDirection {
    Inbound,
    Outbound,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NetworkProtocol {
    Tcp,
    Udp,
    Dns,
    Http,
    Https,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NetworkDecision {
    Allowed,
    Denied,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum NetworkPolicyReportError {
    #[error("eval network policy report enforcement must be true")]
    EnforcementMissing,
    #[error("eval network policy report must not record payloads")]
    PayloadRecordingForbidden,
    #[error("eval network policy report reason must not be empty")]
    MissingReason,
    #[error("eval network policy report policy does not match the required policy")]
    PolicyMismatch,
    #[error("eval network policy report grants do not match the required allowlist")]
    GrantMismatch,
    #[error("invalid eval network allowlist hostname `{host}`")]
    InvalidAllowlistHost { host: String },
    #[error("eval network connection metadata reason must not be empty")]
    MissingConnectionReason,
    #[error("eval network policy report allowed a connection outside the required policy")]
    UnexpectedAllowedConnection,
}

fn validate_grants(
    grants: &[NetworkPolicyGrant],
    expected: &EvalNetworkPolicy,
) -> Result<(), NetworkPolicyReportError> {
    let expected_hosts = expected
        .network_allowlist
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>();
    let mut actual_hosts = BTreeSet::new();
    for grant in grants {
        if grant.direction != NetworkDirection::Outbound {
            return Err(NetworkPolicyReportError::GrantMismatch);
        }
        actual_hosts.insert(normalize_dns_name(&grant.host)?);
    }
    if actual_hosts == expected_hosts {
        Ok(())
    } else {
        Err(NetworkPolicyReportError::GrantMismatch)
    }
}

fn validate_connection(
    connection: &NetworkConnectionMetadata,
    expected: &EvalNetworkPolicy,
) -> Result<(), NetworkPolicyReportError> {
    if connection.reason.trim().is_empty() {
        return Err(NetworkPolicyReportError::MissingConnectionReason);
    }
    if connection.decision == NetworkDecision::Denied {
        return Ok(());
    }
    match connection.direction {
        NetworkDirection::Inbound if expected.inbound == EvalNetworkAccess::Deny => {
            Err(NetworkPolicyReportError::UnexpectedAllowedConnection)
        }
        NetworkDirection::Outbound => {
            let Some(host) = connection.host.as_deref() else {
                return Err(NetworkPolicyReportError::UnexpectedAllowedConnection);
            };
            let normalized = normalize_dns_name(host)?;
            if expected.outbound == EvalNetworkAccess::Allowlist
                && expected.network_allowlist.contains(&normalized)
            {
                Ok(())
            } else {
                Err(NetworkPolicyReportError::UnexpectedAllowedConnection)
            }
        }
        NetworkDirection::Inbound => Ok(()),
    }
}

fn normalize_dns_allowlist(allowlist: &[String]) -> Result<Vec<String>, NetworkPolicyReportError> {
    let mut seen = BTreeSet::new();
    let mut normalized = Vec::new();
    for host in allowlist {
        let host = normalize_dns_name(host)?;
        if seen.insert(host.clone()) {
            normalized.push(host);
        }
    }
    Ok(normalized)
}

fn normalize_dns_name(value: &str) -> Result<String, NetworkPolicyReportError> {
    let candidate = value.trim().trim_end_matches('.').to_ascii_lowercase();
    if candidate.is_empty()
        || candidate.len() > 253
        || candidate.contains(['/', ':', '@', '*', '?', '#'])
        || candidate.parse::<std::net::IpAddr>().is_ok()
        || candidate == "localhost"
        || candidate.ends_with(".localhost")
    {
        return Err(NetworkPolicyReportError::InvalidAllowlistHost {
            host: value.to_string(),
        });
    }
    let labels = candidate.split('.').collect::<Vec<_>>();
    if labels.len() < 2
        || labels.iter().any(|label| {
            label.is_empty()
                || label.len() > 63
                || label.starts_with('-')
                || label.ends_with('-')
                || !label
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        })
    {
        return Err(NetworkPolicyReportError::InvalidAllowlistHost {
            host: value.to_string(),
        });
    }
    Ok(candidate)
}
