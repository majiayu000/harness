#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum FailureClass {
    ZeroOutputSpawnFailure,
    QuotaInteractiveWait,
    CliMissingFile,
    WorktreeCollision,
    StructuredOutputMissing,
    StructuredOutputInvalid,
    SandboxPermission,
    Unclassified,
}
impl FailureClass {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::ZeroOutputSpawnFailure => "zero-output-spawn-failure",
            Self::QuotaInteractiveWait => "quota-interactive-wait",
            Self::CliMissingFile => "cli-missing-file",
            Self::WorktreeCollision => "worktree-collision",
            Self::StructuredOutputMissing => "structured-output-missing",
            Self::StructuredOutputInvalid => "structured-output-invalid",
            Self::SandboxPermission => "sandbox-permission",
            Self::Unclassified => "unclassified",
        }
    }
    pub(crate) fn trips_runtime_profile_breaker(self) -> bool {
        matches!(
            self,
            Self::ZeroOutputSpawnFailure | Self::QuotaInteractiveWait
        )
    }
}
pub(crate) fn classify_agent_failure(error: &str) -> FailureClass {
    let lower = error.to_ascii_lowercase();
    if lower.contains("zero_output_spawn_failure")
        || lower.contains("zero-output spawn failure")
        || lower.contains("completed without assistant output")
    {
        return FailureClass::ZeroOutputSpawnFailure;
    }
    if harness_core::error::is_quota_failure_message(error) {
        return FailureClass::QuotaInteractiveWait;
    }
    if error.contains("No such file or directory") {
        return FailureClass::CliMissingFile;
    }
    if error.contains("WorktreeCollision") {
        return FailureClass::WorktreeCollision;
    }
    if lower.contains("no harness-activity-result") {
        return FailureClass::StructuredOutputMissing;
    }
    if lower.contains("structured activity result was invalid")
        || lower.contains("invalid_structured_output")
        || lower.contains("activity result json")
    {
        return FailureClass::StructuredOutputInvalid;
    }
    if lower.contains("sandbox") || lower.contains("permission denied") {
        return FailureClass::SandboxPermission;
    }
    FailureClass::Unclassified
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime_circuit_breaker::RuntimeCircuitBreakerRegistry;
    use chrono::Utc;
    use harness_core::config::workflow_circuit_breaker::RuntimeCircuitBreakerPolicy;

    #[test]
    fn twenty_task_failures_do_not_open_runtime_profile_breaker() {
        let non_runtime_classes = [
            FailureClass::CliMissingFile,
            FailureClass::WorktreeCollision,
            FailureClass::StructuredOutputMissing,
            FailureClass::StructuredOutputInvalid,
            FailureClass::SandboxPermission,
            FailureClass::Unclassified,
        ];

        for class in non_runtime_classes {
            let registry =
                RuntimeCircuitBreakerRegistry::new(RuntimeCircuitBreakerPolicy::default());
            let now = Utc::now();

            for index in 0..20 {
                assert!(registry
                    .record_failure("codex", &format!("task-job-{index}"), class, now)
                    .is_empty());
            }

            assert!(registry.defer_open_profiles(now).is_empty());
            assert!(registry.snapshots(now).is_empty());
        }
    }
}
