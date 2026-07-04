use harness_core::config::isolation::{IsolationConfig, IsolationTier, IsolationTrustClass};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct IsolationTaskMetadata {
    pub author_trust_class: Option<IsolationTrustClass>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IsolationTierResolution {
    pub tier: IsolationTier,
    pub reason: String,
    pub trust_class: IsolationTrustClass,
}

pub fn resolve_isolation_tier(
    metadata: IsolationTaskMetadata,
    config: &IsolationConfig,
) -> IsolationTierResolution {
    let trust_class = metadata.author_trust_class.unwrap_or_default();
    let tier = config.tier_for_trust_class(trust_class);
    let reason = if config.rules.iter().any(|rule| rule.trust == trust_class) {
        format!(
            "trust_class:{} matched configured isolation rule",
            trust_class_label(trust_class)
        )
    } else {
        format!(
            "trust_class:{} used default isolation tier",
            trust_class_label(trust_class)
        )
    };

    IsolationTierResolution {
        tier,
        reason,
        trust_class,
    }
}

fn trust_class_label(trust_class: IsolationTrustClass) -> &'static str {
    match trust_class {
        IsolationTrustClass::Trusted => "trusted",
        IsolationTrustClass::NonCollaborator => "non_collaborator",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_core::config::isolation::IsolationRule;

    #[test]
    fn tier_resolution_defaults_to_host_for_missing_metadata() {
        let resolution = resolve_isolation_tier(
            IsolationTaskMetadata::default(),
            &IsolationConfig::default(),
        );

        assert_eq!(resolution.tier, IsolationTier::Host);
        assert_eq!(resolution.trust_class, IsolationTrustClass::Trusted);
        assert!(resolution.reason.contains("default isolation tier"));
    }

    #[test]
    fn tier_resolution_uses_trust_class_rule() {
        let config = IsolationConfig {
            default_tier: IsolationTier::Host,
            rules: vec![IsolationRule {
                trust: IsolationTrustClass::NonCollaborator,
                tier: IsolationTier::Container,
            }],
            network_allowlist: Vec::new(),
        };

        let resolution = resolve_isolation_tier(
            IsolationTaskMetadata {
                author_trust_class: Some(IsolationTrustClass::NonCollaborator),
            },
            &config,
        );

        assert_eq!(resolution.tier, IsolationTier::Container);
        assert_eq!(resolution.trust_class, IsolationTrustClass::NonCollaborator);
        assert!(resolution
            .reason
            .contains("matched configured isolation rule"));
    }
}
