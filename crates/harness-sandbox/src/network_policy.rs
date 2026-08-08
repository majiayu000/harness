use harness_core::config::agents::SandboxMode;
#[cfg(any(target_os = "linux", test))]
use harness_core::error::SandboxError;

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
