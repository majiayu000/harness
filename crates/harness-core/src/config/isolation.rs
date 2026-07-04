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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IsolationTrustClass {
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
