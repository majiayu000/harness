//! Classification of `codex exec` failures into harness error kinds.

use super::parse_codex_exec_output;

pub(super) fn codex_structured_error_from_stdout(stdout: &str) -> Option<String> {
    parse_codex_exec_output(stdout).ok()?.structured_error
}

pub(super) fn codex_structured_error(
    message: impl Into<String>,
) -> harness_core::error::HarnessError {
    let message = format!("codex structured error: {}", message.into());
    if harness_core::error::is_billing_failure_message(&message) {
        return harness_core::error::HarnessError::BillingFailed(message);
    }
    if harness_core::error::is_quota_failure_message(&message) {
        return harness_core::error::HarnessError::QuotaExhausted(message);
    }
    if is_codex_authentication_failure_message(&message) {
        return harness_core::error::HarnessError::Config(message);
    }
    harness_core::error::HarnessError::Upstream(message)
}

fn is_codex_authentication_failure_message(message: &str) -> bool {
    let lower = message.to_lowercase();
    lower.contains("authentication failed")
        || lower.contains("not authenticated")
        || lower.contains("unauthorized")
        || lower.contains("invalid api key")
        || lower.contains("invalid_api_key")
        || lower.contains("missing api key")
}

pub(super) fn codex_nonzero_exit_error(
    status: std::process::ExitStatus,
    stderr: &str,
    structured_error: Option<&str>,
) -> harness_core::error::HarnessError {
    if harness_core::error::is_billing_failure_message(stderr) {
        return harness_core::error::HarnessError::BillingFailed(format!(
            "codex billing failure (exit {status}): {stderr}"
        ));
    }
    if harness_core::error::is_quota_failure_message(stderr) {
        return harness_core::error::HarnessError::QuotaExhausted(format!(
            "codex quota exhausted (exit {status}): {stderr}"
        ));
    }

    if let Some(message) = structured_error {
        let mut error = codex_structured_error(format!("exit {status}: {message}"));
        if matches!(error, harness_core::error::HarnessError::Upstream(_))
            && !stderr.trim().is_empty()
        {
            error =
                harness_core::error::HarnessError::Upstream(format!("{error}; stderr=[{stderr}]"));
        }
        return error;
    }

    harness_core::error::HarnessError::AgentExecution(format!(
        "codex exited with {status}: {stderr}"
    ))
}
