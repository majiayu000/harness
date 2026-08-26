use anyhow::Context;
use serde_json::Value;
use std::path::Path;

pub(crate) const PINNED_CHANGE_SCOPE_CLASSIFIER_POLICY_FIELD: &str =
    harness_workflow::runtime::PINNED_CHANGE_SCOPE_CLASSIFIER_POLICY_FIELD;

pub(crate) fn pin_change_scope_classifier_policy(
    project_root: &Path,
    mut data: Value,
) -> anyhow::Result<Value> {
    let config =
        harness_core::config::workflow::load_workflow_config(project_root).with_context(|| {
            format!(
                "change-scope classifier policy could not load for project `{}`",
                project_root.display()
            )
        })?;
    let object = data
        .as_object_mut()
        .context("change-scope classifier policy requires object workflow data")?;
    if object.contains_key(PINNED_CHANGE_SCOPE_CLASSIFIER_POLICY_FIELD) {
        return Ok(data);
    }
    let policy = config
        .activities
        .get(harness_workflow::runtime::CHANGE_SCOPE_REVIEW_ACTIVITY)
        .and_then(|policy| policy.classifier.as_ref().map(|_| policy))
        .context("workflow is missing the required classify_change_scope classifier policy")?;
    let value = serde_json::to_value(policy)
        .context("change-scope classifier policy could not be serialized")?;
    object.insert(
        PINNED_CHANGE_SCOPE_CLASSIFIER_POLICY_FIELD.to_string(),
        value,
    );
    Ok(data)
}

pub(crate) fn merge_runtime_retry_policy(project_root: &Path, mut data: Value) -> Value {
    let config = match harness_core::config::workflow::load_workflow_config(project_root) {
        Ok(config) => config,
        Err(error) => {
            tracing::warn!(
                project = %project_root.display(),
                "workflow runtime retry policy load failed: {error}"
            );
            return data;
        }
    };
    let Some(object) = data.as_object_mut() else {
        return data;
    };
    if !config.runtime_retry_policy.is_empty() {
        match serde_json::to_value(config.runtime_retry_policy) {
            Ok(value) => {
                object.insert("runtime_retry_policy".to_string(), value);
            }
            Err(error) => {
                tracing::warn!(
                    project = %project_root.display(),
                    "workflow runtime retry policy serialization failed: {error}"
                );
            }
        }
    }
    data
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn workflow(rule: &str) -> String {
        format!(
            "---\nactivities:\n  classify_change_scope:\n    classifier:\n      verdicts: [allow]\n      allow:\n        - {rule}\n---\n"
        )
    }

    #[test]
    fn change_scope_policy_is_pinned_on_first_merge() -> anyhow::Result<()> {
        let project = tempfile::tempdir()?;
        std::fs::write(
            project.path().join("WORKFLOW.md"),
            workflow("Original rule"),
        )?;
        let data = pin_change_scope_classifier_policy(project.path(), json!({}))?;

        std::fs::write(project.path().join("WORKFLOW.md"), workflow("Mutated rule"))?;
        let data = pin_change_scope_classifier_policy(project.path(), data)?;

        assert_eq!(
            data[PINNED_CHANGE_SCOPE_CLASSIFIER_POLICY_FIELD]["classifier"]["allow"][0],
            "Original rule"
        );
        Ok(())
    }

    #[test]
    fn missing_classifier_policy_fails_instead_of_returning_unpinned_data() -> anyhow::Result<()> {
        let project = tempfile::tempdir()?;
        std::fs::write(
            project.path().join("WORKFLOW.md"),
            "---\nactivities: {}\n---\n",
        )?;

        let error = pin_change_scope_classifier_policy(project.path(), json!({}))
            .expect_err("missing required classifier policy must fail closed");

        assert!(error.to_string().contains("required classify_change_scope"));
        Ok(())
    }

    #[test]
    fn retry_policy_merge_does_not_backfill_classifier_policy() -> anyhow::Result<()> {
        let project = tempfile::tempdir()?;
        std::fs::write(project.path().join("WORKFLOW.md"), workflow("Current rule"))?;

        let data = merge_runtime_retry_policy(project.path(), json!({"legacy": true}));

        assert!(data
            .get(PINNED_CHANGE_SCOPE_CLASSIFIER_POLICY_FIELD)
            .is_none());
        Ok(())
    }
}
