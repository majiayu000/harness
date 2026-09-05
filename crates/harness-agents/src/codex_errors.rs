//! Classification of `codex exec` failures into harness error kinds.

use super::codex_exec_parser::{CodexStructuredErrorKind, ParsedCodexExecOutput};
use super::parse_codex_exec_output;

pub(super) struct CodexStructuredError {
    message: String,
    kind: CodexStructuredErrorKind,
}

pub(super) fn codex_structured_error_from_stdout(stdout: &str) -> Option<CodexStructuredError> {
    structured_error_from_parsed(&parse_codex_exec_output(stdout).ok()?)
}

pub(super) fn codex_structured_error(
    message: impl Into<String>,
    kind: CodexStructuredErrorKind,
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
    match kind {
        CodexStructuredErrorKind::Provider => harness_core::error::HarnessError::Upstream(message),
        CodexStructuredErrorKind::Permanent => {
            harness_core::error::HarnessError::AgentExecution(message)
        }
    }
}

pub(super) fn codex_explicit_failure_error(
    parsed: ParsedCodexExecOutput,
) -> harness_core::error::HarnessError {
    codex_structured_error(
        parsed
            .structured_error
            .unwrap_or_else(|| "codex turn failed".to_string()),
        parsed
            .structured_error_kind
            .unwrap_or(CodexStructuredErrorKind::Permanent),
    )
}

pub(super) fn codex_nonzero_exit_error_from_parsed(
    status: std::process::ExitStatus,
    stderr: &str,
    parsed: &ParsedCodexExecOutput,
) -> harness_core::error::HarnessError {
    let structured_error = structured_error_from_parsed(parsed);
    codex_nonzero_exit_error(status, stderr, structured_error.as_ref())
}

fn structured_error_from_parsed(parsed: &ParsedCodexExecOutput) -> Option<CodexStructuredError> {
    Some(CodexStructuredError {
        message: parsed.structured_error.clone()?,
        kind: parsed.structured_error_kind?,
    })
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
    structured_error: Option<&CodexStructuredError>,
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

    if let Some(structured_error) = structured_error {
        let error = codex_structured_error(
            format!("exit {status}: {}", structured_error.message),
            structured_error.kind,
        );
        if stderr.trim().is_empty() {
            return error;
        }
        return append_stderr(error, stderr);
    }

    harness_core::error::HarnessError::AgentExecution(format!(
        "codex exited with {status}: {stderr}"
    ))
}

fn append_stderr(
    error: harness_core::error::HarnessError,
    stderr: &str,
) -> harness_core::error::HarnessError {
    use harness_core::error::HarnessError;

    match error {
        HarnessError::AgentExecution(message) => {
            HarnessError::AgentExecution(format!("{message}; stderr=[{stderr}]"))
        }
        HarnessError::Upstream(message) => {
            HarnessError::Upstream(format!("{message}; stderr=[{stderr}]"))
        }
        HarnessError::Config(message) => {
            HarnessError::Config(format!("{message}; stderr=[{stderr}]"))
        }
        HarnessError::BillingFailed(message) => {
            HarnessError::BillingFailed(format!("{message}; stderr=[{stderr}]"))
        }
        HarnessError::QuotaExhausted(message) => {
            HarnessError::QuotaExhausted(format!("{message}; stderr=[{stderr}]"))
        }
        other => other,
    }
}
