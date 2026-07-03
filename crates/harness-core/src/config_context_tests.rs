use super::misc::{ContextConfig, ContextMode};
use super::HarnessConfig;

#[test]
fn harness_config_defaults_include_context_shadow_mode() {
    let config = HarnessConfig::default();

    assert_eq!(config.context.mode, ContextMode::Shadow);
    assert_eq!(config.context.budget_tokens, 24_000);
    assert_eq!(config.context.reserved_headroom, 0.20);
    assert_eq!(config.context.provider_timeout_ms, 2_000);
    assert_eq!(config.context.quotas.rule, 0.30);
    assert_eq!(config.context.quotas.skill, 0.25);
    assert_eq!(config.context.quotas.contract, 0.25);
    assert_eq!(config.context.quotas.brief, 0.15);
    assert_eq!(config.context.quotas.draft, 0.05);
}

#[test]
fn context_config_deserializes_from_toml() {
    let toml_str = r#"
        mode = "enforce"
        budget_tokens = 12000
        reserved_headroom = 0.10
        provider_timeout_ms = 500

        [quotas]
        rule = 0.40
        skill = 0.20
        contract = 0.20
        brief = 0.15
        draft = 0.05
    "#;

    let config: ContextConfig = toml::from_str(toml_str).expect("context config should parse");

    assert_eq!(config.mode, ContextMode::Enforce);
    assert_eq!(config.budget_tokens, 12_000);
    assert_eq!(config.reserved_headroom, 0.10);
    assert_eq!(config.provider_timeout_ms, 500);
    assert_eq!(config.quotas.rule, 0.40);
    assert_eq!(config.quotas.skill, 0.20);
    assert_eq!(config.quotas.contract, 0.20);
    assert_eq!(config.quotas.brief, 0.15);
    assert_eq!(config.quotas.draft, 0.05);
}
