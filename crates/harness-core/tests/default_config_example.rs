use harness_core::agent::AgentEgressMode;
use harness_core::config::agents::AgentPermissionMode;
use harness_core::config::isolation::IsolationTier;
use harness_core::config::HarnessConfig;

#[test]
fn shipped_example_keeps_the_default_codex_provider_reachable() {
    let config: HarnessConfig =
        toml::from_str(include_str!("../../../config/default.toml.example"))
            .expect("shipped default config example should parse");

    assert_eq!(config.agents.default_agent, "codex");
    assert_eq!(
        config.agents.resolve_permission_mode(),
        AgentPermissionMode::Scoped
    );
    assert_eq!(config.isolation.default_tier, IsolationTier::Container);
    assert!(config
        .isolation
        .network_allowlist
        .iter()
        .any(|host| host == "api.openai.com"));
    assert_eq!(
        AgentEgressMode::resolve(
            config.agents.resolve_permission_mode(),
            &config.isolation.network_allowlist,
        ),
        AgentEgressMode::FirstPartyProxy
    );
}
