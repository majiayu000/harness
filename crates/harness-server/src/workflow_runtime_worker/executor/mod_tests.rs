#[cfg(test)]
mod tests {
    use super::runtime_timeout::runtime_timeout_fallback;
    use super::*;
    use harness_core::config::workflow::RuntimeDispatchProfileOverride;
    use harness_workflow::runtime::{RuntimeKind, RuntimeProfile, WorkflowSubject};
    use serde_json::json;
    fn runtime_job(activity: &str) -> RuntimeJob {
        RuntimeJob::pending(
            "command-1",
            RuntimeKind::CodexJsonrpc,
            "codex-default",
            json!({ "activity": activity }),
        )
    }
    fn workflow(definition_id: &str) -> WorkflowInstance {
        WorkflowInstance::new(
            definition_id,
            1,
            "running",
            WorkflowSubject::new("issue", "issue-1"),
        )
    }
    #[test]
    fn runtime_timeout_fallback_prefers_workflow_activity_profile() {
        let mut config = WorkflowConfig::default();
        config.runtime_dispatch.timeout_secs = Some(900);
        config.runtime_dispatch.activity_profiles.insert(
            "inspect_pr_feedback".to_string(),
            RuntimeDispatchProfileOverride {
                timeout_secs: Some(1800),
                ..RuntimeDispatchProfileOverride::default()
            },
        );
        config.runtime_dispatch.workflow_profiles.insert(
            "pr_feedback".to_string(),
            RuntimeDispatchProfileOverride {
                timeout_secs: Some(240),
                ..RuntimeDispatchProfileOverride::default()
            },
        );
        config
            .runtime_dispatch
            .workflow_activity_profiles
            .entry("pr_feedback".to_string())
            .or_default()
            .insert(
                "inspect_pr_feedback".to_string(),
                RuntimeDispatchProfileOverride {
                    timeout_secs: Some(120),
                    ..RuntimeDispatchProfileOverride::default()
                },
            );
        assert_eq!(
            runtime_timeout_fallback(
                &config,
                Some(&workflow("pr_feedback")),
                &runtime_job("inspect_pr_feedback"),
            ),
            Some(120)
        );
    }

    #[test]
    fn runtime_profile_with_timeout_fallback_preserves_embedded_timeout() {
        let mut config = WorkflowConfig::default();
        config.runtime_dispatch.timeout_secs = Some(900);
        let mut profile = RuntimeProfile::new("codex-default", RuntimeKind::CodexJsonrpc);
        profile.timeout_secs = Some(42);
        let profile = runtime_profile_with_timeout_fallback(
            profile,
            &config,
            Some(&workflow("pr_feedback")),
            &runtime_job("inspect_pr_feedback"),
        );

        assert_eq!(profile.timeout_secs, Some(42));
    }

    #[test]
    fn runtime_timeout_fallback_has_global_activity_defaults() {
        let config = WorkflowConfig::default();
        assert_eq!(
            runtime_timeout_fallback(
                &config,
                Some(&workflow("pr_feedback")),
                &runtime_job("inspect_pr_feedback")
            ),
            Some(3600)
        );
        assert_eq!(
            runtime_timeout_fallback(&config, None, &runtime_job("implement_issue")),
            Some(3600)
        );
    }

    #[test]
    fn runtime_worker_disabled_result_for_config_cancels_agent_work() {
        let mut config = WorkflowConfig::default();
        config.runtime_worker.enabled = false;
        let Some(result) = runtime_worker_disabled_result_for_config(
            "implement_issue",
            Path::new("/tmp/project"),
            &config,
        ) else {
            panic!("disabled runtime worker should produce a preflight result");
        };

        assert_eq!(
            result.status,
            harness_workflow::runtime::ActivityStatus::Cancelled
        );
        assert_eq!(result.activity, "implement_issue");
        assert!(result
            .summary
            .contains("Runtime worker is disabled for project /tmp/project"));
    }

    #[test]
    fn runtime_worker_disabled_result_for_config_allows_enabled_project() {
        let config = WorkflowConfig::default();

        assert!(runtime_worker_disabled_result_for_config(
            "implement_issue",
            Path::new("/tmp/project"),
            &config,
        )
        .is_none());
    }

    #[test]
    fn runtime_workspace_finalization_failure_marks_activity_failed() {
        let result =
            ActivityResult::succeeded("implement_issue", "Created a pull request.").with_artifact(
                ActivityArtifact::new("pull_request", json!({ "pr_number": 42 })),
            );
        let result = combine_activity_result_with_runtime_workspace_finalization(
            Ok(result),
            Err(anyhow::anyhow!("after_run hook failed")),
        )
        .expect("finalization failure should be returned as a failed activity result");
        assert_eq!(result.activity, "implement_issue");
        assert_eq!(
            result.status,
            harness_workflow::runtime::ActivityStatus::Failed
        );
        assert_eq!(
            result.error_kind,
            Some(harness_workflow::runtime::ActivityErrorKind::Retryable)
        );
        assert!(result
            .summary
            .contains("Runtime workspace finalization failed"));
        assert!(result
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("after_run hook failed"));
        assert!(result
            .artifacts
            .iter()
            .any(|artifact| artifact.artifact_type == "pull_request"));
        assert!(result.artifacts.iter().any(|artifact| {
            artifact.artifact_type == "runtime_workspace_finalization_warning"
                && artifact.artifact["error"] == "after_run hook failed"
        }));
    }

    #[test]
    fn runtime_workspace_finalization_failure_preserves_failed_activity_result() {
        let result = ActivityResult::failed(
            "address_pr_feedback",
            "Structured output was invalid.",
            "fatal",
        )
        .with_error_kind(harness_workflow::runtime::ActivityErrorKind::Fatal)
        .with_artifact(ActivityArtifact::new(
            "activity_result_parse_error",
            json!({ "field": "status" }),
        ));
        let result = combine_activity_result_with_runtime_workspace_finalization(
            Ok(result),
            Err(anyhow::anyhow!("after_run hook failed")),
        )
        .expect("failed activity result should be preserved");
        assert_eq!(result.activity, "address_pr_feedback");
        assert_eq!(
            result.status,
            harness_workflow::runtime::ActivityStatus::Failed
        );
        assert_eq!(result.summary, "Structured output was invalid.");
        assert_eq!(result.error.as_deref(), Some("fatal"));
        assert_eq!(
            result.error_kind,
            Some(harness_workflow::runtime::ActivityErrorKind::Fatal)
        );
        assert!(result
            .artifacts
            .iter()
            .any(|artifact| artifact.artifact_type == "activity_result_parse_error"));
        assert!(result.artifacts.iter().any(|artifact| {
            artifact.artifact_type == "runtime_workspace_finalization_warning"
                && artifact.artifact["error"] == "after_run hook failed"
        }));
    }

    #[test]
    fn internal_non_agent_activity_includes_server_owned_pr_inspection() {
        assert!(is_internal_non_agent_activity(&runtime_job(
            "inspect_pr_feedback"
        )));
        assert!(!is_internal_non_agent_activity(&runtime_job(
            "implement_issue"
        )));
    }
}
