use super::*;

#[test]
fn feishu_debug_redacts_secrets() {
    let config = FeishuIntakeConfig {
        enabled: true,
        app_id: Some("app-123".to_string()),
        app_secret: Some("super-secret".to_string()),
        verification_token: Some("tok-abc".to_string()),
        trigger_keyword: "harness".to_string(),
        default_repo: Some("owner/repo".to_string()),
    };
    let debug_output = format!("{config:?}");
    assert!(
        !debug_output.contains("app-123"),
        "app_id must not appear in Debug output"
    );
    assert!(
        !debug_output.contains("super-secret"),
        "app_secret must not appear in Debug output"
    );
    assert!(
        !debug_output.contains("tok-abc"),
        "verification_token must not appear in Debug output"
    );
    assert!(
        debug_output.contains("[REDACTED]"),
        "Debug output must contain [REDACTED]"
    );
}

#[test]
fn feishu_debug_shows_none_for_absent_secrets() {
    let config = FeishuIntakeConfig::default();
    let debug_output = format!("{config:?}");
    assert!(
        debug_output.contains("None"),
        "absent secrets should show as None"
    );
}

#[test]
fn github_find_repo_config_prefers_exact_multi_repo_match() {
    let config = GitHubIntakeConfig {
        repo: "owner/default".to_string(),
        repos: vec![
            GitHubRepoConfig {
                repo: "owner/default".to_string(),
                label: "default".to_string(),
                project_root: Some("/srv/default".to_string()),
                auto_merge: None,
                auto_recovery: None,
                merge_method: None,
                delete_branch: None,
                require_review_threads_resolved: None,
                require_clean_merge_state: None,
            },
            GitHubRepoConfig {
                repo: "owner/other".to_string(),
                label: "other".to_string(),
                project_root: Some("/srv/other".to_string()),
                auto_merge: None,
                auto_recovery: None,
                merge_method: None,
                delete_branch: None,
                require_review_threads_resolved: None,
                require_clean_merge_state: None,
            },
        ],
        ..GitHubIntakeConfig::default()
    };

    let repo = config
        .find_repo_config("owner/other")
        .expect("repo config should exist");
    assert_eq!(repo.repo, "owner/other");
    assert_eq!(repo.label, "other");
    assert_eq!(repo.project_root.as_deref(), Some("/srv/other"));
}

#[test]
fn github_find_repo_config_supports_single_repo_shorthand() {
    let config = GitHubIntakeConfig {
        repo: "owner/default".to_string(),
        label: "triage".to_string(),
        ..GitHubIntakeConfig::default()
    };

    let repo = config
        .find_repo_config("owner/default")
        .expect("repo config should exist");
    assert_eq!(repo.repo, "owner/default");
    assert_eq!(repo.label, "triage");
    assert_eq!(repo.project_root, None);
}

#[test]
fn github_discovery_driver_defaults_to_direct_rest() {
    let config = GitHubIntakeConfig::default();

    assert_eq!(
        config.discovery_driver,
        GitHubPollDiscoveryDriver::DirectRest
    );
    assert_eq!(config.discovery_driver.to_string(), "direct_rest");
}

#[test]
fn github_discovery_driver_parses_agent() -> Result<(), toml::de::Error> {
    let config: GitHubIntakeConfig = toml::from_str(
        r#"
enabled = true
discovery_driver = "agent"
repo = "owner/repo"
"#,
    )?;

    assert_eq!(config.discovery_driver, GitHubPollDiscoveryDriver::Agent);
    Ok(())
}

#[test]
fn github_auto_merge_policy_requires_explicit_opt_in_and_allows_repo_override() {
    let config: GitHubIntakeConfig = toml::from_str(
        r#"
enabled = true

[auto_merge]
enabled = false
method = "squash"
delete_branch = true
require_review_threads_resolved = true
require_clean_merge_state = true
merge_execution = "agent"
verify_merge_completion = true

[[repos]]
repo = "owner/auto"
label = "harness"
auto_merge = true
merge_method = "rebase"
delete_branch = false
require_review_threads_resolved = false
require_clean_merge_state = false

[[repos]]
repo = "owner/manual"
label = "harness"
"#,
    )
    .expect("config should parse");

    let auto = config.auto_merge_policy_for_repo("owner/auto");
    assert!(auto.enabled);
    assert_eq!(auto.method, GitHubMergeMethod::Rebase);
    assert!(!auto.delete_branch);
    assert!(!auto.require_review_threads_resolved);
    assert!(!auto.require_clean_merge_state);
    assert_eq!(auto.merge_execution, GitHubMergeExecution::Agent);
    assert!(auto.verify_merge_completion);

    let manual = config.auto_merge_policy_for_repo("owner/manual");
    assert!(!manual.enabled);
    assert_eq!(manual.method, GitHubMergeMethod::Squash);
    assert!(manual.delete_branch);
    assert!(manual.require_review_threads_resolved);
    assert!(manual.require_clean_merge_state);
    assert_eq!(manual.merge_execution, GitHubMergeExecution::Agent);
    assert!(manual.verify_merge_completion);
}
#[test]
fn intake_auto_recovery_defaults_are_off_and_bounded() {
    let config = GitHubIntakeConfig::default();
    assert!(!config.auto_recovery.enabled);
    assert_eq!(config.auto_recovery.max_attempts, 3);
    assert_eq!(config.auto_recovery.initial_backoff_secs, 300);
    assert_eq!(config.auto_recovery.max_backoff_secs, 14400);
    assert!((config.auto_recovery.jitter_ratio - 0.2).abs() < f64::EPSILON);
    assert_eq!(config.auto_recovery.tick_interval_secs, 60);
    config
        .auto_recovery
        .validate()
        .expect("defaults must validate");
}

#[test]
fn intake_auto_recovery_repo_override_beats_global_flag() {
    let config: GitHubIntakeConfig = toml::from_str(
        r#"
enabled = true

[auto_recovery]
enabled = false

[[repos]]
repo = "owner/opted-in"
auto_recovery = true

[[repos]]
repo = "owner/opted-out"
auto_recovery = false

[[repos]]
repo = "owner/inherits"
"#,
    )
    .expect("config should parse");
    assert!(config.auto_recovery_enabled_for_repo("owner/opted-in"));
    assert!(!config.auto_recovery_enabled_for_repo("owner/opted-out"));
    assert!(!config.auto_recovery_enabled_for_repo("owner/inherits"));
    // Unconfigured repos never opt in, regardless of the global flag.
    assert!(!config.auto_recovery_enabled_for_repo("owner/unknown"));
}

#[test]
fn intake_auto_recovery_global_flag_applies_when_repo_does_not_override() {
    let config: GitHubIntakeConfig = toml::from_str(
        r#"
enabled = true

[auto_recovery]
enabled = true

[[repos]]
repo = "owner/inherits"
"#,
    )
    .expect("config should parse");
    assert!(config.auto_recovery_enabled_for_repo("owner/inherits"));
}

#[test]
fn intake_auto_recovery_validation_rejects_nonsensical_values() {
    // B-016: invalid policy values are rejected at load, not recheck time.
    let base = GitHubAutoRecoveryConfig {
        enabled: true,
        ..GitHubAutoRecoveryConfig::default()
    };
    base.validate().expect("enabled defaults must validate");

    let zero_attempts = GitHubAutoRecoveryConfig {
        max_attempts: 0,
        ..base.clone()
    };
    assert!(zero_attempts
        .validate()
        .unwrap_err()
        .to_string()
        .contains("max_attempts"));

    let too_many_attempts = GitHubAutoRecoveryConfig {
        max_attempts: AUTO_RECOVERY_MAX_ATTEMPTS_CEILING + 1,
        ..base.clone()
    };
    assert!(too_many_attempts.validate().is_err());

    let ceiling_below_floor = GitHubAutoRecoveryConfig {
        initial_backoff_secs: 600,
        max_backoff_secs: 300,
        ..base.clone()
    };
    assert!(ceiling_below_floor
        .validate()
        .unwrap_err()
        .to_string()
        .contains("max_backoff_secs"));

    for jitter_ratio in [-0.1, 1.1, f64::NAN] {
        let bad_jitter = GitHubAutoRecoveryConfig {
            jitter_ratio,
            ..base.clone()
        };
        assert!(
            bad_jitter.validate().is_err(),
            "jitter_ratio {jitter_ratio} must be rejected"
        );
    }

    let zero_tick = GitHubAutoRecoveryConfig {
        tick_interval_secs: 0,
        ..base.clone()
    };
    assert!(zero_tick.validate().is_err());
}

#[test]
fn intake_auto_recovery_disabled_skips_bound_validation() {
    // Disabled policy is inert; invalid tunables must not block startup.
    let disabled = GitHubAutoRecoveryConfig {
        enabled: false,
        max_attempts: 0,
        jitter_ratio: 5.0,
        ..GitHubAutoRecoveryConfig::default()
    };
    disabled
        .validate()
        .expect("disabled policy is not validated");
}
