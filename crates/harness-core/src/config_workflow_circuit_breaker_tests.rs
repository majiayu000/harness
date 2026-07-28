use super::{
    workflow::{load_workflow_config, load_workflow_document_with_base},
    HarnessConfig,
};

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

fn replace_first_toml_assignment(input: &str, prefix: &str, replacement: &str) -> String {
    let mut replaced = false;
    let output = input
        .lines()
        .map(|line| {
            if !replaced && line.starts_with(prefix) {
                replaced = true;
                replacement
            } else {
                line
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(replaced, "missing TOML assignment starting with {prefix:?}");
    output
}

fn yaml_mapping_with(field: &str, value: serde_yaml::Value) -> serde_yaml::Mapping {
    let mut mapping = serde_yaml::Mapping::new();
    mapping.insert(serde_yaml::Value::String(field.to_owned()), value);
    mapping
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
fn harness_config_ignores_legacy_and_extension_root_keys() {
    let toml_str = format!(
        r#"
        retired_q_value = 0.75

        [rule_enforcer_extension]
        mode = "audit"

        {}
        "#,
        workflow_breaker_harness_config_toml("")
    );

    let config: HarnessConfig = toml::from_str(&toml_str)
        .expect("retired and extension root keys must remain upgrade-compatible");
    assert!(config.workflow.completion_evidence_enforced);
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
fn harness_config_rejects_root_completion_evidence_key() {
    let toml_str = format!(
        "completion_evidence_enforced = false\n{}",
        workflow_breaker_harness_config_toml("")
    );

    let error = toml::from_str::<HarnessConfig>(&toml_str)
        .expect_err("a misplaced root kill-switch key must fail visibly");
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
fn harness_config_rejects_completion_evidence_key_under_server() {
    let toml_str = workflow_breaker_harness_config_toml("").replacen(
        "[agents]",
        "completion_evidence_enforced = false\n\n[agents]",
        1,
    );

    let error = toml::from_str::<HarnessConfig>(&toml_str)
        .expect_err("a kill switch under [server] must fail visibly");
    assert!(error
        .to_string()
        .contains("unknown field `completion_evidence_enforced`"));
}

#[test]
fn harness_config_rejects_legacy_key_in_nested_extension() {
    let toml_str = workflow_breaker_harness_config_toml(
        r#"
        [operator_extension.nested]
        runtime_completion_evidence_enforced = false
        "#,
    );

    let error = toml::from_str::<HarnessConfig>(&toml_str)
        .expect_err("a legacy kill switch in a nested extension must fail visibly");
    assert!(error
        .to_string()
        .contains("unknown field `runtime_completion_evidence_enforced`"));
}

#[test]
fn harness_config_retains_unrelated_nested_extension_compatibility() {
    let toml_str = workflow_breaker_harness_config_toml(
        r#"
        [operator_extension.nested]
        mode = "audit"
        "#,
    )
    .replacen(
        "[agents]",
        "operator_extension_mode = \"audit\"\n\n[agents]",
        1,
    );

    let config = toml::from_str::<HarnessConfig>(&toml_str)
        .expect("unrelated fields in known and extension tables must remain compatible");
    assert!(config.workflow.completion_evidence_enforced);
}

#[test]
fn harness_config_json_and_yaml_null_round_trip() {
    let config = HarnessConfig::default();

    let json = serde_json::to_string(&config).expect("default config must serialize as JSON");
    assert!(json.contains("\"database_url\":null"));
    let from_json: HarnessConfig =
        serde_json::from_str(&json).expect("JSON null options must round-trip");
    assert!(from_json.server.database_url.is_none());

    let yaml = serde_yaml::to_string(&config).expect("default config must serialize as YAML");
    assert!(yaml.contains("database_url: null"));
    let from_yaml: HarnessConfig =
        serde_yaml::from_str(&yaml).expect("YAML null options must round-trip");
    assert!(from_yaml.server.database_url.is_none());
}

#[test]
fn harness_config_preserves_native_toml_datetime_type_errors_and_spans() {
    let serialized =
        toml::to_string_pretty(&HarnessConfig::default()).expect("default config must serialize");
    let cases = [
        (
            replace_first_toml_assignment(
                &serialized,
                "quiet_window_start =",
                "quiet_window_start = 06:00:00",
            ),
            "expected a formatted time string",
        ),
        (
            replace_first_toml_assignment(&serialized, "data_dir =", "data_dir = 1979-05-27"),
            "expected path string",
        ),
        (
            replace_first_toml_assignment(
                &serialized,
                "default_agent =",
                "default_agent = 1979-05-27",
            ),
            "expected a string",
        ),
    ];

    for (input, expected) in cases {
        let error = toml::from_str::<HarnessConfig>(&input)
            .expect_err("native TOML datetime values must retain typed-field rejection");
        let message = error.to_string();
        assert!(message.contains(expected), "{message}");
        assert!(message.contains("TOML parse error at line"), "{message}");
    }
}

#[test]
fn harness_config_rejects_literal_dotted_reserved_keys() {
    let base = workflow_breaker_harness_config_toml("");
    let cases = [
        format!("\"workflow.completion_evidence_enforced\" = false\n{base}"),
        format!("[\"operator.runtime_completion_evidence_enforced\"]\nmode = \"audit\"\n{base}"),
    ];

    for input in cases {
        let error = toml::from_str::<HarnessConfig>(&input)
            .expect_err("literal dotted reserved keys must not mimic canonical placement");
        assert!(error.to_string().contains("unknown field `"), "{error}");
    }
}

#[test]
fn harness_config_rejects_reserved_key_in_known_table_ignored_subtree() {
    let input = workflow_breaker_harness_config_toml("").replacen(
        "[agents]",
        r#"[server.operator_extension]
values = [{ nested = { completion_evidence_enforced = false } }]

[agents]"#,
        1,
    );

    let error = toml::from_str::<HarnessConfig>(&input)
        .expect_err("ignored subtrees under known tables must be scanned recursively");
    assert!(error
        .to_string()
        .contains("unknown field `completion_evidence_enforced`"));
}

#[test]
fn harness_config_accepts_unrelated_datetime_in_known_table_ignored_subtree() {
    let input = workflow_breaker_harness_config_toml("").replacen(
        "[agents]",
        r#"[server.operator_extension]
values = [{ observed_at = 1979-05-27T07:32:00Z, mode = "audit" }]

[agents]"#,
        1,
    );

    let config = toml::from_str::<HarnessConfig>(&input)
        .expect("recursive ignored-value scanning must preserve unrelated TOML values");
    assert!(config.workflow.completion_evidence_enforced);
}

#[test]
fn harness_config_scans_json_arrays_in_extensions() {
    let mut json =
        serde_json::to_value(HarnessConfig::default()).expect("default config must serialize");
    json.as_object_mut()
        .expect("config must serialize as a map")
        .insert(
            "operator_extension".to_string(),
            serde_json::json!({
                "values": [{"completion_evidence_enforced": false}]
            }),
        );
    let json_error = serde_json::from_value::<HarnessConfig>(json)
        .expect_err("reserved keys in JSON extension arrays must fail");
    assert!(json_error
        .to_string()
        .contains("unknown field `completion_evidence_enforced`"));
}

#[test]
fn harness_config_scans_compound_yaml_keys_in_extensions() {
    let compound_keys = [
        (
            serde_yaml::Value::Mapping(yaml_mapping_with(
                "completion_evidence_enforced",
                serde_yaml::Value::Bool(false),
            )),
            "completion_evidence_enforced",
        ),
        (
            serde_yaml::Value::Sequence(vec![serde_yaml::Value::Mapping(yaml_mapping_with(
                "completion_evidence_enforced",
                serde_yaml::Value::Bool(false),
            ))]),
            "completion_evidence_enforced",
        ),
        (
            serde_yaml::Value::Sequence(vec![
                serde_yaml::Value::String("completion_evidence_enforced".to_owned()),
                serde_yaml::Value::String("mode".to_owned()),
            ]),
            "completion_evidence_enforced",
        ),
        (
            serde_yaml::Value::Sequence(vec![serde_yaml::Value::String(
                "runtime_completion_evidence_enforced".to_owned(),
            )]),
            "runtime_completion_evidence_enforced",
        ),
        (
            serde_yaml::Value::Mapping(yaml_mapping_with(
                "mode",
                serde_yaml::Value::String("completion_evidence_enforced".to_owned()),
            )),
            "completion_evidence_enforced",
        ),
    ];

    for (case_index, (compound_key, field)) in compound_keys.into_iter().enumerate() {
        let mut yaml =
            serde_yaml::to_value(HarnessConfig::default()).expect("default config must serialize");
        let mut extension = serde_yaml::Mapping::new();
        extension.insert(compound_key, serde_yaml::Value::Null);
        yaml.as_mapping_mut()
            .expect("config must serialize as a map")
            .insert(
                serde_yaml::Value::String("operator_extension".to_owned()),
                serde_yaml::Value::Mapping(extension),
            );

        let error = serde_yaml::from_value::<HarnessConfig>(yaml)
            .expect_err("compound extension keys must not hide reserved fields");
        assert!(
            error
                .to_string()
                .contains(&format!("unknown field `{field}`")),
            "compound-key case {case_index}: {error}"
        );
    }
}

#[test]
fn reserved_key_deserializer_scans_tagged_compound_map_keys() {
    use serde::de::IntoDeserializer as _;

    let tagged_key = serde_yaml::Value::Tagged(Box::new(serde_yaml::value::TaggedValue {
        tag: serde_yaml::value::Tag::new("scope"),
        value: serde_yaml::Value::Mapping(yaml_mapping_with(
            "mode",
            serde_yaml::Value::String("completion_evidence_enforced".to_owned()),
        )),
    }));
    let mut yaml = serde_yaml::Mapping::new();
    yaml.insert(tagged_key, serde_yaml::Value::Null);

    let error = super::reserved_key_deserializer::deserialize::<_, serde::de::IgnoredAny>(
        serde_yaml::Value::Mapping(yaml).into_deserializer(),
    )
    .expect_err("tagged compound keys must not hide reserved fields");
    assert!(error
        .to_string()
        .contains("unknown field `completion_evidence_enforced`"));

    let tagged_key = serde_yaml::Value::Tagged(Box::new(serde_yaml::value::TaggedValue {
        tag: serde_yaml::value::Tag::new("scope"),
        value: serde_yaml::Value::Mapping(yaml_mapping_with(
            "mode",
            serde_yaml::Value::String("audit".to_owned()),
        )),
    }));
    let mut yaml = serde_yaml::Mapping::new();
    yaml.insert(tagged_key, serde_yaml::Value::Null);
    super::reserved_key_deserializer::deserialize::<_, serde::de::IgnoredAny>(
        serde_yaml::Value::Mapping(yaml).into_deserializer(),
    )
    .expect("unrelated tagged compound keys must remain compatible");
}

#[test]
fn harness_config_accepts_unrelated_compound_yaml_keys_in_extensions() {
    let mut nested = serde_yaml::Mapping::new();
    nested.insert(
        serde_yaml::Value::String("mode".to_owned()),
        serde_yaml::Value::String("audit".to_owned()),
    );
    let compound_keys = [
        serde_yaml::Value::Mapping(nested.clone()),
        serde_yaml::Value::Sequence(vec![serde_yaml::Value::Mapping(nested)]),
        serde_yaml::Value::Sequence(vec![
            serde_yaml::Value::String("mode".to_owned()),
            serde_yaml::Value::String("audit".to_owned()),
        ]),
    ];

    for compound_key in compound_keys {
        let mut yaml =
            serde_yaml::to_value(HarnessConfig::default()).expect("default config must serialize");
        let mut extension = serde_yaml::Mapping::new();
        extension.insert(compound_key, serde_yaml::Value::Null);
        extension.insert(
            serde_yaml::Value::String("literal_value".to_owned()),
            serde_yaml::Value::String("completion_evidence_enforced".to_owned()),
        );
        yaml.as_mapping_mut()
            .expect("config must serialize as a map")
            .insert(
                serde_yaml::Value::String("operator_extension".to_owned()),
                serde_yaml::Value::Mapping(extension),
            );

        let config = serde_yaml::from_value::<HarnessConfig>(yaml)
            .expect("unrelated compound extension keys must remain compatible");
        assert!(config.workflow.completion_evidence_enforced);
    }
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
fn project_workflow_ignores_retired_and_extension_root_sections() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    std::fs::write(
        dir.path().join("WORKFLOW.md"),
        r#"---
workflow:
  id: compatibility-test
repo_backlog:
  enabled: true
operator_extension:
  mode: audit
---
"#,
    )?;

    let config = load_workflow_config(dir.path())
        .expect("retired and extension sections must remain upgrade-compatible");
    assert_eq!(config.workflow.id.as_deref(), Some("compatibility-test"));
    Ok(())
}

#[test]
fn project_workflow_rejects_root_completion_evidence_key() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    std::fs::write(
        dir.path().join("WORKFLOW.md"),
        r#"---
completion_evidence_enforced: false
---
"#,
    )?;

    let error = load_workflow_config(dir.path())
        .expect_err("a deployment-global switch at the project root must fail visibly");
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
fn project_workflow_rejects_completion_evidence_key_in_nested_extension() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    std::fs::write(
        dir.path().join("WORKFLOW.md"),
        r#"---
operator_extension:
  nested:
    completion_evidence_enforced: false
---
"#,
    )?;

    let error = load_workflow_config(dir.path())
        .expect_err("a nested project kill switch must fail visibly");
    assert!(error
        .to_string()
        .contains("unknown field `completion_evidence_enforced`"));
    Ok(())
}

#[test]
fn project_workflow_rejects_legacy_key_in_nested_extension() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    std::fs::write(
        dir.path().join("WORKFLOW.md"),
        r#"---
operator_extension:
  nested:
    runtime_completion_evidence_enforced: false
---
"#,
    )?;

    let error = load_workflow_config(dir.path())
        .expect_err("a nested legacy project kill switch must fail visibly");
    assert!(error
        .to_string()
        .contains("unknown field `runtime_completion_evidence_enforced`"));
    Ok(())
}

#[test]
fn project_workflow_retains_unrelated_nested_extension_compatibility() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    std::fs::write(
        dir.path().join("WORKFLOW.md"),
        r#"---
workflow:
  id: nested-extension-compatibility
operator_extension:
  nested:
    mode: audit
---
"#,
    )?;

    let config = load_workflow_config(dir.path())
        .expect("unrelated nested extension fields must remain compatible");
    assert_eq!(
        config.workflow.id.as_deref(),
        Some("nested-extension-compatibility")
    );
    Ok(())
}

#[test]
fn project_and_base_workflows_reject_reserved_keys_in_compound_keys() -> anyhow::Result<()> {
    let cases = [
        (
            "operator_extension:\n  values:\n    - completion_evidence_enforced: false",
            "completion_evidence_enforced",
        ),
        (
            "operator_extension:\n  nested: !audit\n    runtime_completion_evidence_enforced: false",
            "runtime_completion_evidence_enforced",
        ),
        (
            "operator_extension:\n  nested:\n    ? !reserved completion_evidence_enforced\n    : false",
            "completion_evidence_enforced",
        ),
        (
            "\"workflow.completion_evidence_enforced\": false",
            "completion_evidence_enforced",
        ),
        (
            "operator_extension:\n  ? [completion_evidence_enforced, mode]\n  : null",
            "completion_evidence_enforced",
        ),
        (
            "operator_extension:\n  ? {mode: runtime_completion_evidence_enforced}\n  : null",
            "runtime_completion_evidence_enforced",
        ),
        (
            "operator_extension:\n  ? !scope {mode: completion_evidence_enforced}\n  : null",
            "completion_evidence_enforced",
        ),
    ];

    for (front_matter, field) in cases {
        let dir = tempfile::tempdir()?;
        std::fs::write(
            dir.path().join("WORKFLOW.md"),
            format!("---\n{front_matter}\n---\n"),
        )?;
        let error = load_workflow_config(dir.path())
            .expect_err("reserved keys in any project workflow shape must fail");
        assert!(
            error
                .to_string()
                .contains(&format!("unknown field `{field}`")),
            "{error}"
        );

        let base_dir = tempfile::tempdir()?;
        let project_dir = tempfile::tempdir()?;
        let base_path = base_dir.path().join("WORKFLOW.md");
        std::fs::write(&base_path, format!("---\n{front_matter}\n---\n"))?;
        std::fs::write(
            project_dir.path().join("WORKFLOW.md"),
            "---\noperator_extension: replaced\n---\n",
        )?;
        let error = load_workflow_document_with_base(project_dir.path(), Some(&base_path))
            .expect_err("a repo override must not hide a reserved key in the base workflow");
        assert!(
            error
                .to_string()
                .contains(&format!("unknown field `{field}`")),
            "{error}"
        );
    }
    Ok(())
}

#[test]
fn project_workflow_accepts_unrelated_tagged_extension() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    std::fs::write(
        dir.path().join("WORKFLOW.md"),
        "---\nworkflow:\n  id: tagged-extension\noperator_extension: !audit\n  values: [one, two]\n---\n",
    )?;

    let config = load_workflow_config(dir.path())
        .expect("unrelated tagged extension values must remain compatible");
    assert_eq!(config.workflow.id.as_deref(), Some("tagged-extension"));
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
