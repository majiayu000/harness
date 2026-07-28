use super::{workflow::load_workflow_config, HarnessConfig};

fn workflow_breaker_harness_config_toml(workflow_section: &str) -> String {
    format!(
        r#"
        [server]
        transport = "http"
        http_addr = "127.0.0.1:9800"
        data_dir = "/tmp/harness-workflow-breaker"
        project_root = "/tmp/project"

        [agents]
        default_agent = "codex"
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

        {workflow_section}
        "#
    )
}

#[test]
fn workflow_circuit_breaker_config_defaults() {
    let config = HarnessConfig::default();
    assert!(config.workflow.completion_evidence_enforced);
    let breaker = &config.workflow.circuit_breaker;

    assert!(breaker.enabled);
    assert_eq!(breaker.consecutive_failures, 5);
    assert_eq!(breaker.distinct_runtime_jobs, 3);
    assert_eq!(breaker.failure_window_secs, 300);
    assert_eq!(breaker.cooldown_secs, 600);
    assert_eq!(breaker.backoff_factor, 2.0);
    assert_eq!(breaker.max_cooldown_secs, 7200);
}

#[test]
fn harness_config_deserializes_completion_evidence_kill_switch() {
    let toml_str = workflow_breaker_harness_config_toml(
        r#"
        [workflow]
        completion_evidence_enforced = false
        "#,
    );

    let config: HarnessConfig = toml::from_str(&toml_str).unwrap();
    assert!(!config.workflow.completion_evidence_enforced);
}

#[test]
fn harness_config_rejects_unknown_completion_evidence_key() {
    let toml_str = workflow_breaker_harness_config_toml(
        r#"
        [workflow]
        runtime_completion_evidence_enforced = false
        "#,
    );

    let error = toml::from_str::<HarnessConfig>(&toml_str)
        .expect_err("an unknown kill-switch key must fail visibly");
    assert!(error
        .to_string()
        .contains("unknown field `runtime_completion_evidence_enforced`"));
}

#[test]
fn harness_config_rejects_completion_evidence_key_inside_circuit_breaker() {
    let toml_str = workflow_breaker_harness_config_toml(
        r#"
        [workflow.circuit_breaker]
        completion_evidence_enforced = false
        "#,
    );

    let error = toml::from_str::<HarnessConfig>(&toml_str)
        .expect_err("a kill switch nested under the circuit breaker must fail visibly");
    assert!(error
        .to_string()
        .contains("unknown field `completion_evidence_enforced`"));
}

#[test]
fn harness_config_rejects_legacy_root_completion_evidence_key() {
    let toml_str = format!(
        "runtime_completion_evidence_enforced = false\n{}",
        workflow_breaker_harness_config_toml("")
    );

    let error = toml::from_str::<HarnessConfig>(&toml_str)
        .expect_err("a legacy root kill-switch key must fail visibly");
    assert!(error
        .to_string()
        .contains("unknown field `runtime_completion_evidence_enforced`"));
}

#[test]
fn project_workflow_rejects_deployment_completion_evidence_key() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    std::fs::write(
        dir.path().join("WORKFLOW.md"),
        r#"---
runtime_worker:
  completion_evidence_enforced: false
---
"#,
    )?;

    let error = load_workflow_config(dir.path())
        .expect_err("the deployment-global switch must not be accepted in project config");
    assert!(error
        .to_string()
        .contains("unknown field `completion_evidence_enforced`"));
    Ok(())
}

#[test]
fn project_workflow_rejects_completion_evidence_key_under_workflow() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    std::fs::write(
        dir.path().join("WORKFLOW.md"),
        r#"---
workflow:
  completion_evidence_enforced: false
---
"#,
    )?;

    let error = load_workflow_config(dir.path())
        .expect_err("a deployment-global switch nested under workflow must fail visibly");
    assert!(error
        .to_string()
        .contains("unknown field `completion_evidence_enforced`"));
    Ok(())
}

#[test]
fn project_workflow_rejects_legacy_root_completion_evidence_key() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    std::fs::write(
        dir.path().join("WORKFLOW.md"),
        r#"---
runtime_completion_evidence_enforced: false
---
"#,
    )?;

    let error = load_workflow_config(dir.path())
        .expect_err("the legacy project kill-switch key must fail visibly");
    assert!(error
        .to_string()
        .contains("unknown field `runtime_completion_evidence_enforced`"));
    Ok(())
}

#[test]
fn harness_config_deserializes_workflow_circuit_breaker() {
    let toml_str = workflow_breaker_harness_config_toml(
        r#"
        [workflow.circuit_breaker]
        enabled = false
        consecutive_failures = 7
        distinct_runtime_jobs = 4
        failure_window_secs = 240
        cooldown_secs = 120
        backoff_factor = 1.5
        max_cooldown_secs = 900
        "#,
    );

    let config: HarnessConfig = toml::from_str(&toml_str).unwrap();
    let breaker = &config.workflow.circuit_breaker;

    assert!(!breaker.enabled);
    assert_eq!(breaker.consecutive_failures, 7);
    assert_eq!(breaker.distinct_runtime_jobs, 4);
    assert_eq!(breaker.failure_window_secs, 240);
    assert_eq!(breaker.cooldown_secs, 120);
    assert_eq!(breaker.backoff_factor, 1.5);
    assert_eq!(breaker.max_cooldown_secs, 900);
}
