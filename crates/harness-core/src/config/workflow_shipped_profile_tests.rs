use super::*;

#[test]
fn shipped_scope_classifier_uses_a_supported_exact_model_profile() -> anyhow::Result<()> {
    let config_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../config");
    let config = load_workflow_config(&config_dir)?;
    let profile = config
        .runtime_dispatch
        .activity_profiles
        .get("classify_change_scope")
        .ok_or_else(|| anyhow::anyhow!("shipped scope classifier profile is missing"))?;

    assert_eq!(profile.runtime_kind.as_deref(), Some("claude_code"));
    assert_eq!(
        profile.runtime_profile.as_deref(),
        Some("classifier-claude")
    );
    assert_eq!(profile.model.as_deref(), Some("claude-sonnet-4-6"));
    assert_eq!(profile.sandbox.as_deref(), Some("read-only"));
    Ok(())
}
