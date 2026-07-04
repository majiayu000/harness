use harness_core::config::misc::{OtelConfig, OtelExporter};

#[test]
fn otel_trajectory_flags_deserialize_and_default_off() -> anyhow::Result<()> {
    let default_config = OtelConfig::default();
    assert_eq!(default_config.exporter, OtelExporter::Disabled);
    assert!(!default_config.trajectory);
    assert!(!default_config.capture_content);

    let config: OtelConfig = toml::from_str(
        r#"
        trajectory = true
        capture_content = true
        "#,
    )?;
    assert!(config.trajectory);
    assert!(config.capture_content);
    Ok(())
}
