use serde_json::Value;
use std::path::Path;

pub(crate) fn merge_runtime_retry_policy(project_root: &Path, mut data: Value) -> Value {
    let policy = match harness_core::config::workflow::load_workflow_config(project_root) {
        Ok(config) if !config.runtime_retry_policy.is_empty() => config.runtime_retry_policy,
        Ok(_) => return data,
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
    match serde_json::to_value(policy) {
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
    data
}
