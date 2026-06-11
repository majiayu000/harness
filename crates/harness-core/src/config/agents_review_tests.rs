use super::*;

#[test]
fn review_config_defaults_to_local_first_provider_ids() {
    let config = AgentReviewConfig::default();

    assert!(config.enabled);
    assert_eq!(config.strategy, ReviewStrategy::LocalFirst);
    assert_eq!(config.required_providers, vec!["codex_cli_review"]);
    assert_eq!(
        config.advisory_providers,
        vec!["gemini_github_bot", "codex_github_bot"]
    );
    assert!(!config.external_required);
    assert!(!config.publish_local_review_comment);
    assert!(config.codex_cli_review.enabled);
    assert_eq!(config.codex_cli_review.cli_path, PathBuf::from("codex"));
    assert_eq!(config.codex_cli_review.base_ref, "origin/main");
    assert_eq!(config.codex_cli_review.timeout_secs, 1800);
    assert!(!config.codex_agent_review.enabled);
    assert_eq!(config.gemini_github_bot.trigger_command, "/gemini review");
    assert_eq!(
        config.gemini_github_bot.reviewer_name,
        "gemini-code-assist[bot]"
    );
    assert_eq!(config.codex_github_bot.trigger_command, "@codex");
    assert_eq!(config.review_wait_budget_secs, 3600);
}

#[test]
fn review_config_deserializes_provider_settings() {
    let toml_str = r#"
        enabled = true
        strategy = "local_first"
        required_providers = ["codex_cli_review"]
        advisory_providers = ["gemini_github_bot"]
        external_required = true
        publish_local_review_comment = true
        review_wait_budget_secs = 1800

        [codex_cli_review]
        enabled = true
        cli_path = "/opt/bin/codex"
        model = "gpt-5.5"
        reasoning_effort = "xhigh"
        base_ref = "main"
        timeout_secs = 2400
        output_format = "json"

        [gemini_github_bot]
        enabled = true
        trigger_command = "/gemini review"
        reviewer_name = "gemini-code-assist[bot]"
        auto_trigger = false
        wait_secs = 120
        max_rounds = 1
    "#;
    let config: AgentReviewConfig = toml::from_str(toml_str).unwrap();

    assert_eq!(config.strategy, ReviewStrategy::LocalFirst);
    assert_eq!(config.required_providers, vec!["codex_cli_review"]);
    assert_eq!(config.advisory_providers, vec!["gemini_github_bot"]);
    assert!(config.external_required);
    assert!(config.publish_local_review_comment);
    assert_eq!(config.review_wait_budget_secs, 1800);
    assert_eq!(
        config.codex_cli_review.cli_path,
        PathBuf::from("/opt/bin/codex")
    );
    assert_eq!(config.codex_cli_review.model, "gpt-5.5");
    assert_eq!(config.codex_cli_review.reasoning_effort, "xhigh");
    assert_eq!(config.codex_cli_review.base_ref, "main");
    assert_eq!(config.codex_cli_review.timeout_secs, 2400);
    assert_eq!(config.gemini_github_bot.wait_secs, 120);
    assert_eq!(config.gemini_github_bot.max_rounds, 1);
}

#[test]
fn review_config_maps_legacy_fallback_chain_to_explicit_advisory_providers() {
    let toml_str = r#"
        enabled = true
        fallback_chain = ["codex", "gemini"]
    "#;
    let config: AgentReviewConfig = toml::from_str(toml_str).unwrap();

    assert_eq!(config.fallback_chain, vec!["codex", "gemini"]);
    assert_eq!(
        config.advisory_providers,
        vec!["codex_github_bot", "gemini_github_bot"]
    );
}

#[test]
fn review_config_normalizes_legacy_fallback_chain_case_and_whitespace(
) -> Result<(), toml::de::Error> {
    let toml_str = r#"
        enabled = true
        fallback_chain = [" Gemini ", "CODEX", " Custom_Provider "]
    "#;
    let config: AgentReviewConfig = toml::from_str(toml_str)?;

    assert_eq!(
        config.advisory_providers,
        vec!["gemini_github_bot", "codex_github_bot", "custom_provider"]
    );
    Ok(())
}

#[test]
fn review_config_partial_external_provider_inherits_provider_defaults() {
    let toml_str = r#"
        enabled = true

        [codex_github_bot]
        wait_secs = 60
    "#;
    let config: AgentReviewConfig = toml::from_str(toml_str).unwrap();

    assert_eq!(config.codex_github_bot.trigger_command, "@codex");
    assert_eq!(
        config.codex_github_bot.reviewer_name,
        "chatgpt-codex-connector[bot]"
    );
    assert_eq!(config.codex_github_bot.wait_secs, 60);
}

#[test]
fn review_config_partial_providers_inherit_legacy_review_defaults() -> Result<(), toml::de::Error> {
    let toml_str = r#"
        enabled = false
        reviewer_agent = "claude"
        max_rounds = 5
        review_bot_command = "/alt review"
        reviewer_name = "alt-reviewer[bot]"

        [codex_agent_review]
        enabled = true

        [gemini_github_bot]
        wait_secs = 60
    "#;
    let config: AgentReviewConfig = toml::from_str(toml_str)?;

    assert!(config.codex_agent_review.enabled);
    assert_eq!(config.codex_agent_review.reviewer_agent, "claude");
    assert_eq!(config.codex_agent_review.max_rounds, 5);
    assert_eq!(config.gemini_github_bot.trigger_command, "/alt review");
    assert_eq!(config.gemini_github_bot.reviewer_name, "alt-reviewer[bot]");
    assert!(config.gemini_github_bot.auto_trigger);
    assert_eq!(config.gemini_github_bot.wait_secs, 60);
    Ok(())
}

#[test]
fn review_config_disabled_local_review_defaults_to_hosted_review() {
    let toml_str = r#"
        enabled = false
    "#;
    let config: AgentReviewConfig = toml::from_str(toml_str).unwrap();

    assert!(!config.enabled);
    assert!(config.required_providers.is_empty());
    assert!(config.review_bot_auto_trigger);
    assert!(config.gemini_github_bot.auto_trigger);
    assert!(config.codex_github_bot.auto_trigger);
}
