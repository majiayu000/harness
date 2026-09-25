use super::{
    workflow::{load_workflow_config, load_workflow_document_with_base},
    HarnessConfig,
};

const COMPLETION_EVIDENCE_FIELD: &str = "completion_evidence_enforced";
const LEGACY_COMPLETION_EVIDENCE_FIELD: &str = "runtime_completion_evidence_enforced";

fn tagged_mapping_key(tag: &str, value: &str) -> serde_yaml::Value {
    let mut mapping = serde_yaml::Mapping::new();
    mapping.insert(
        serde_yaml::Value::String("mode".to_owned()),
        serde_yaml::Value::String(value.to_owned()),
    );
    serde_yaml::Value::Tagged(Box::new(serde_yaml::value::TaggedValue {
        tag: serde_yaml::value::Tag::new(tag),
        value: serde_yaml::Value::Mapping(mapping),
    }))
}

fn harness_config_with_unknown_extension_key(key: serde_yaml::Value) -> serde_yaml::Value {
    let mut config =
        serde_yaml::to_value(HarnessConfig::default()).expect("default config must serialize");
    let mut extension = serde_yaml::Mapping::new();
    extension.insert(key, serde_yaml::Value::Null);
    config
        .as_mapping_mut()
        .expect("config must serialize as a mapping")
        .insert(
            serde_yaml::Value::String("operator_extension".to_owned()),
            serde_yaml::Value::Mapping(extension),
        );
    config
}

#[test]
fn harness_config_accepts_unrelated_tagged_unknown_extension() {
    let config = harness_config_with_unknown_extension_key(tagged_mapping_key("scope", "audit"));

    serde_yaml::from_value::<HarnessConfig>(config)
        .expect("an unrelated tagged extension must retain generic Serde compatibility");
}

#[test]
fn harness_config_rejects_reserved_content_in_tagged_unknown_extension() {
    let cases = [
        (
            COMPLETION_EVIDENCE_FIELD,
            LEGACY_COMPLETION_EVIDENCE_FIELD,
            COMPLETION_EVIDENCE_FIELD,
        ),
        (
            LEGACY_COMPLETION_EVIDENCE_FIELD,
            COMPLETION_EVIDENCE_FIELD,
            LEGACY_COMPLETION_EVIDENCE_FIELD,
        ),
        (
            "scope",
            COMPLETION_EVIDENCE_FIELD,
            COMPLETION_EVIDENCE_FIELD,
        ),
    ];

    for (tag, value, expected_first_match) in cases {
        let config = harness_config_with_unknown_extension_key(tagged_mapping_key(tag, value));
        let error = serde_yaml::from_value::<HarnessConfig>(config)
            .expect_err("reserved tagged-key content must fail closed");

        assert!(
            error
                .to_string()
                .contains(&format!("unknown field `{expected_first_match}`")),
            "tag {tag:?} with value {value:?}: {error}"
        );
    }
}

fn tagged_key_workflow(tag: &str, value: &str) -> String {
    format!("---\noperator_extension:\n  ? !{tag} {{mode: {value}}}\n  : null\n---\n")
}

fn tagged_value_workflow(tag: &str, value: &str) -> String {
    format!("---\noperator_extension: !{tag}\n  mode: {value}\n---\n")
}

fn assert_repo_and_base_reject(workflow: &str, expected_first_match: &str) -> anyhow::Result<()> {
    let repo = tempfile::tempdir()?;
    std::fs::write(repo.path().join("WORKFLOW.md"), workflow)?;
    let error = load_workflow_config(repo.path())
        .expect_err("the repository WORKFLOW.md must reject reserved tagged-key content");
    assert!(
        error
            .to_string()
            .contains(&format!("unknown field `{expected_first_match}`")),
        "repository WORKFLOW.md: {error}"
    );

    let base = tempfile::tempdir()?;
    let project = tempfile::tempdir()?;
    let base_path = base.path().join("WORKFLOW.md");
    std::fs::write(&base_path, workflow)?;
    std::fs::write(
        project.path().join("WORKFLOW.md"),
        "---\noperator_extension: replaced\n---\n",
    )?;
    let error = load_workflow_document_with_base(project.path(), Some(&base_path))
        .expect_err("the base WORKFLOW.md must reject reserved tagged-key content");
    assert!(
        error
            .to_string()
            .contains(&format!("unknown field `{expected_first_match}`")),
        "base WORKFLOW.md: {error}"
    );
    Ok(())
}

#[test]
fn repo_and_base_workflows_scan_tag_before_tagged_key_payload() -> anyhow::Result<()> {
    let cases = [
        (COMPLETION_EVIDENCE_FIELD, LEGACY_COMPLETION_EVIDENCE_FIELD),
        (LEGACY_COMPLETION_EVIDENCE_FIELD, COMPLETION_EVIDENCE_FIELD),
    ];

    for (tag, later_payload_match) in cases {
        assert_repo_and_base_reject(&tagged_key_workflow(tag, later_payload_match), tag)?;
    }
    Ok(())
}

#[test]
fn repo_and_base_workflows_accept_reserved_text_in_tagged_values() -> anyhow::Result<()> {
    let workflow =
        tagged_value_workflow(COMPLETION_EVIDENCE_FIELD, LEGACY_COMPLETION_EVIDENCE_FIELD);

    let repo = tempfile::tempdir()?;
    std::fs::write(repo.path().join("WORKFLOW.md"), &workflow)?;
    load_workflow_config(repo.path())
        .expect("reserved text in an ordinary tagged value is not a configuration key");

    let base = tempfile::tempdir()?;
    let project = tempfile::tempdir()?;
    let base_path = base.path().join("WORKFLOW.md");
    std::fs::write(&base_path, workflow)?;
    load_workflow_document_with_base(project.path(), Some(&base_path))
        .expect("the base workflow must apply the same key-only scan");
    Ok(())
}
