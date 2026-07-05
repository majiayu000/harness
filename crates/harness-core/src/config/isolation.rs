use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IsolationTier {
    #[default]
    Host,
    Container,
    Microvm,
}

impl IsolationTier {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Host => "host",
            Self::Container => "container",
            Self::Microvm => "microvm",
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IsolationTrustClass {
    #[default]
    Trusted,
    NonCollaborator,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IsolationRule {
    pub trust: IsolationTrustClass,
    pub tier: IsolationTier,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IsolationConfig {
    #[serde(default)]
    pub default_tier: IsolationTier,
    #[serde(default)]
    pub rules: Vec<IsolationRule>,
    #[serde(default)]
    pub network_allowlist: Vec<String>,
}

impl Default for IsolationConfig {
    fn default() -> Self {
        Self {
            default_tier: IsolationTier::Host,
            rules: Vec::new(),
            network_allowlist: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IsolationTierStatus {
    pub tier: IsolationTier,
    pub available: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl IsolationTierStatus {
    pub fn available(tier: IsolationTier) -> Self {
        Self {
            tier,
            available: true,
            reason: None,
        }
    }

    pub fn unavailable(tier: IsolationTier, reason: impl Into<String>) -> Self {
        Self {
            tier,
            available: false,
            reason: Some(reason.into()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IsolationAvailability {
    #[serde(default)]
    pub tiers: Vec<IsolationTierStatus>,
}

impl Default for IsolationAvailability {
    fn default() -> Self {
        Self {
            tiers: vec![
                IsolationTierStatus::available(IsolationTier::Host),
                IsolationTierStatus::unavailable(
                    IsolationTier::Container,
                    "isolation tier `container` has not been probed",
                ),
                IsolationTierStatus::unavailable(
                    IsolationTier::Microvm,
                    "isolation tier `microvm` is reserved but not implemented",
                ),
            ],
        }
    }
}

impl IsolationAvailability {
    pub fn new(tiers: Vec<IsolationTierStatus>) -> Self {
        Self { tiers }
    }

    pub fn status_for(&self, tier: IsolationTier) -> IsolationTierStatus {
        self.tiers
            .iter()
            .find(|status| status.tier == tier)
            .cloned()
            .unwrap_or_else(|| {
                IsolationTierStatus::unavailable(
                    tier,
                    format!("isolation tier `{}` has not been probed", tier.as_str()),
                )
            })
    }

    pub fn unavailable_required_tiers(&self, config: &IsolationConfig) -> Vec<IsolationTierStatus> {
        config
            .required_tiers()
            .into_iter()
            .map(|tier| self.status_for(tier))
            .filter(|status| !status.available)
            .collect()
    }

    pub fn ensure_tier_available(&self, tier: IsolationTier) -> anyhow::Result<()> {
        let status = self.status_for(tier);
        if status.available {
            return Ok(());
        }
        let reason = status
            .reason
            .as_deref()
            .unwrap_or("tier availability probe failed");
        anyhow::bail!(
            "required isolation tier `{}` is unavailable: {reason}",
            tier.as_str()
        )
    }
}

impl IsolationConfig {
    pub fn tier_for_trust_class(&self, trust: IsolationTrustClass) -> IsolationTier {
        self.rules
            .iter()
            .find(|rule| rule.trust == trust)
            .map(|rule| rule.tier)
            .unwrap_or(self.default_tier)
    }

    pub fn required_tiers(&self) -> BTreeSet<IsolationTier> {
        let mut tiers = BTreeSet::from([self.default_tier]);
        tiers.extend(self.rules.iter().map(|rule| rule.tier));
        tiers
    }

    pub fn validate_startup_support(&self) -> anyhow::Result<()> {
        if self.required_tiers().contains(&IsolationTier::Microvm) {
            anyhow::bail!(
                "isolation tier `microvm` is reserved but not implemented; use `host` or `container`"
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn isolation_config_defaults_to_host_without_rules() {
        let config = IsolationConfig::default();

        assert_eq!(config.default_tier, IsolationTier::Host);
        assert_eq!(
            config.tier_for_trust_class(IsolationTrustClass::NonCollaborator),
            IsolationTier::Host
        );
        assert!(config.network_allowlist.is_empty());
        assert!(config.validate_startup_support().is_ok());
    }

    #[test]
    fn isolation_config_parses_rules_and_network_allowlist() {
        let config: IsolationConfig = toml::from_str(
            r#"
default_tier = "host"
network_allowlist = ["github.com", "api.anthropic.com"]

[[rules]]
trust = "non_collaborator"
tier = "container"
"#,
        )
        .expect("isolation config should parse");

        assert_eq!(config.default_tier, IsolationTier::Host);
        assert_eq!(
            config.tier_for_trust_class(IsolationTrustClass::Trusted),
            IsolationTier::Host
        );
        assert_eq!(
            config.tier_for_trust_class(IsolationTrustClass::NonCollaborator),
            IsolationTier::Container
        );
        assert_eq!(
            config.network_allowlist,
            vec!["github.com".to_string(), "api.anthropic.com".to_string()]
        );
        assert!(config.validate_startup_support().is_ok());
    }

    #[test]
    fn isolation_availability_reports_unavailable_required_tier() {
        let config = IsolationConfig {
            default_tier: IsolationTier::Host,
            rules: vec![IsolationRule {
                trust: IsolationTrustClass::NonCollaborator,
                tier: IsolationTier::Container,
            }],
            network_allowlist: Vec::new(),
        };
        let availability = IsolationAvailability::new(vec![
            IsolationTierStatus::available(IsolationTier::Host),
            IsolationTierStatus::unavailable(IsolationTier::Container, "docker unavailable"),
        ]);

        let unavailable = availability.unavailable_required_tiers(&config);

        assert_eq!(unavailable.len(), 1);
        assert_eq!(unavailable[0].tier, IsolationTier::Container);
        assert!(availability
            .ensure_tier_available(IsolationTier::Container)
            .expect_err("container should be unavailable")
            .to_string()
            .contains("docker unavailable"));
    }

    #[test]
    fn isolation_config_accepts_microvm_in_parser_but_rejects_startup_support() {
        let config: IsolationConfig = toml::from_str(
            r#"
default_tier = "microvm"
"#,
        )
        .expect("reserved tier should parse");

        assert_eq!(config.default_tier, IsolationTier::Microvm);
        let err = config
            .validate_startup_support()
            .expect_err("microvm should fail startup validation");
        assert!(
            err.to_string().contains("microvm"),
            "error should name reserved tier: {err}"
        );
    }

    #[test]
    fn isolation_config_rejects_microvm_rule_at_startup_support() {
        let config: IsolationConfig = toml::from_str(
            r#"
default_tier = "host"

[[rules]]
trust = "non_collaborator"
tier = "microvm"
"#,
        )
        .expect("reserved rule tier should parse");

        assert_eq!(
            config.tier_for_trust_class(IsolationTrustClass::NonCollaborator),
            IsolationTier::Microvm
        );
        let err = config
            .validate_startup_support()
            .expect_err("microvm rule should fail startup validation");
        assert!(
            err.to_string().contains("reserved"),
            "error should explain reserved support: {err}"
        );
    }

    #[test]
    fn isolation_config_deserializes_from_harness_config() -> anyhow::Result<()> {
        let input = r#"
[server]
transport = "http"
http_addr = "127.0.0.1:9800"
data_dir = "/tmp/harness-data"
project_root = "/tmp/project"

[agents]
default_agent = "claude"

[agents.claude]
cli_path = "claude"
default_model = "sonnet"

[agents.codex]
cli_path = "codex"

[agents.anthropic_api]
base_url = "https://api.anthropic.com"
default_model = "claude-sonnet-4-6"

[gc]
max_drafts_per_run = 5
budget_per_signal_usd = 0.5
total_budget_usd = 5.0

[gc.signal_thresholds]
repeated_warn_min = 10
chronic_block_min = 5
hot_file_edits_min = 20
slow_op_threshold_ms = 5000
slow_op_count_min = 10
escalation_ratio = 1.5
violation_min = 5

[rules]
discovery_paths = []

[observe]
session_renewal_secs = 1800
log_retention_max_files = 30
log_retention_days = 90

[otel]

[isolation]
default_tier = "host"
network_allowlist = ["github.com", "api.anthropic.com"]

[[isolation.rules]]
trust = "non_collaborator"
tier = "container"
"#;

        let config: crate::config::HarnessConfig = toml::from_str(input)?;

        assert_eq!(config.isolation.default_tier, IsolationTier::Host);
        assert_eq!(
            config
                .isolation
                .tier_for_trust_class(IsolationTrustClass::NonCollaborator),
            IsolationTier::Container
        );
        assert_eq!(
            config.isolation.network_allowlist,
            vec!["github.com".to_string(), "api.anthropic.com".to_string()]
        );
        Ok(())
    }
}
