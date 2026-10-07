use crate::config::agents::SandboxMode;
use crate::types::{TurnFailure, TurnFailureKind};
use std::path::PathBuf;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum HarnessError {
    #[error("thread not found: {0}")]
    ThreadNotFound(String),

    #[error("turn not found: {0}")]
    TurnNotFound(String),

    #[error("agent not found: {0}")]
    AgentNotFound(String),

    #[error("draft not found: {0}")]
    DraftNotFound(String),

    #[error("skill not found: {0}")]
    SkillNotFound(String),

    #[error("exec plan not found: {0}")]
    ExecPlanNotFound(String),

    #[error("rule not found: {0}")]
    RuleNotFound(String),

    #[error("invalid state: {0}")]
    InvalidState(String),

    #[error("budget exceeded: spent ${spent:.2}, limit ${limit:.2}")]
    BudgetExceeded { spent: f64, limit: f64 },

    #[error("timeout after {0:?}")]
    Timeout(std::time::Duration),

    #[error("agent execution failed: {0}")]
    AgentExecution(String),

    /// HTTP failure with status retained independently of provider error prose.
    #[error("agent execution failed: {message}")]
    AgentHttpResponse {
        provider: String,
        status: u16,
        message: String,
    },

    #[error("agent upstream failure: {0}")]
    Upstream(String),

    #[error("agent quota exhausted: {0}")]
    QuotaExhausted(String),

    /// Permanent billing failure (payment required, insufficient balance).
    /// Unlike `QuotaExhausted`, this will not recover after a wait period.
    #[error("agent billing failure: {0}")]
    BillingFailed(String),

    #[error("protocol error: {0}")]
    Protocol(String),

    #[error("persistence error: {0}")]
    Persistence(String),

    #[error("config error: {0}")]
    Config(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("unsupported: {0}")]
    Unsupported(String),

    #[error(transparent)]
    Sandbox(#[from] SandboxError),

    #[error(transparent)]
    TaskDbDecode(#[from] TaskDbDecodeError),

    #[error("{0}")]
    Other(String),
}

#[derive(Debug, Error)]
pub enum SandboxError {
    #[error("sandbox mode `{mode}` is unsupported on `{platform}`")]
    UnsupportedPlatform {
        mode: SandboxMode,
        platform: &'static str,
    },
    #[error("sandbox tool not found: {0}")]
    MissingTool(&'static str),
    #[error("invalid sandbox path `{path}`: {reason}")]
    InvalidPath { path: PathBuf, reason: &'static str },
    #[error("sandbox helper `{helper}` does not support mode `{mode}`")]
    InvalidHelperMode {
        helper: &'static str,
        mode: SandboxMode,
    },
    #[error("network policy `{policy}` is unsupported by sandbox helper `{helper}`")]
    UnsupportedNetworkPolicy {
        helper: &'static str,
        policy: &'static str,
    },
}

#[derive(Debug, Error)]
pub enum TaskDbDecodeError {
    #[error("failed to deserialize rounds for task `{task_id}`")]
    RoundsDeserialize {
        task_id: String,
        #[source]
        source: serde_json::Error,
    },
    #[error("failed to deserialize depends_on for task `{task_id}`")]
    DependsOnDeserialize {
        task_id: String,
        #[source]
        source: serde_json::Error,
    },
    #[error("failed to deserialize phase for task `{task_id}`")]
    PhaseDeserialize {
        task_id: String,
        #[source]
        source: serde_json::Error,
    },
    #[error("failed to deserialize scheduler_state for task `{task_id}`")]
    SchedulerStateDeserialize {
        task_id: String,
        #[source]
        source: serde_json::Error,
    },
}

pub type Error = HarnessError;
pub type Result<T> = std::result::Result<T, HarnessError>;

const MAX_FAILURE_BODY_EXCERPT_CHARS: usize = 240;

impl HarnessError {
    pub fn turn_failure(&self) -> Option<TurnFailure> {
        match self {
            HarnessError::Timeout(duration) => Some(TurnFailure {
                kind: TurnFailureKind::Timeout,
                provider: None,
                upstream_status: None,
                message: Some(format!("timeout after {duration:?}")),
                body_excerpt: None,
            }),
            HarnessError::QuotaExhausted(message) => Some(TurnFailure {
                kind: TurnFailureKind::Quota,
                provider: provider_from_message(message),
                upstream_status: parse_upstream_status(message),
                message: Some(message.clone()),
                body_excerpt: excerpt_after_colon(message),
            }),
            HarnessError::BillingFailed(message) => Some(TurnFailure {
                kind: TurnFailureKind::Billing,
                provider: provider_from_message(message),
                upstream_status: parse_upstream_status(message),
                message: Some(message.clone()),
                body_excerpt: excerpt_after_colon(message),
            }),
            HarnessError::Upstream(message) => Some(TurnFailure {
                kind: TurnFailureKind::Upstream,
                provider: provider_from_message(message),
                upstream_status: parse_upstream_status(message),
                message: Some(message.clone()),
                body_excerpt: excerpt_after_colon(message),
            }),
            HarnessError::AgentHttpResponse {
                provider,
                status,
                message,
            } => {
                // Keep billing/quota semantics ahead of generic request rejection.
                let kind = if *status == 402 || is_billing_failure_message(message) {
                    TurnFailureKind::Billing
                } else if *status == 429 || is_quota_failure_message(message) {
                    TurnFailureKind::Quota
                } else if matches!(*status, 400 | 401 | 403 | 404 | 413 | 415 | 422) {
                    TurnFailureKind::RequestRejected
                } else {
                    TurnFailureKind::Upstream
                };
                Some(TurnFailure {
                    kind,
                    provider: Some(provider.clone()),
                    upstream_status: Some(*status),
                    message: Some(message.clone()),
                    body_excerpt: excerpt_after_colon(message),
                })
            }
            HarnessError::AgentExecution(message) => {
                Some(classify_agent_execution_failure(message))
            }
            _ => None,
        }
    }
}

fn classify_agent_execution_failure(message: &str) -> TurnFailure {
    if is_billing_failure_message(message) {
        return TurnFailure {
            kind: TurnFailureKind::Billing,
            provider: provider_from_message(message),
            upstream_status: parse_upstream_status(message),
            message: Some(message.to_string()),
            body_excerpt: excerpt_after_colon(message),
        };
    }

    if is_quota_failure_message(message) {
        return TurnFailure {
            kind: TurnFailureKind::Quota,
            provider: provider_from_message(message),
            upstream_status: parse_upstream_status(message),
            message: Some(message.to_string()),
            body_excerpt: excerpt_after_colon(message),
        };
    }

    if message.starts_with("API returned ") {
        return TurnFailure {
            kind: TurnFailureKind::Upstream,
            provider: Some("anthropic-api".to_string()),
            upstream_status: parse_upstream_status(message),
            message: Some(message.to_string()),
            body_excerpt: excerpt_after_colon(message),
        };
    }

    if message.starts_with("API request failed:") {
        return TurnFailure {
            kind: TurnFailureKind::Upstream,
            provider: Some("anthropic-api".to_string()),
            upstream_status: None,
            message: Some(message.to_string()),
            body_excerpt: excerpt_after_colon(message),
        };
    }

    if message.starts_with("Agent stream stalled:") || message.starts_with("Agent turn timed out") {
        return TurnFailure {
            kind: TurnFailureKind::Timeout,
            provider: provider_from_message(message),
            upstream_status: None,
            message: Some(message.to_string()),
            body_excerpt: excerpt_after_colon(message),
        };
    }

    if message.contains("stdout unavailable")
        || message.contains("stream send failed")
        || message.contains("failed reading ")
        || message.contains("failed to serialize")
    {
        return TurnFailure {
            kind: TurnFailureKind::Protocol,
            provider: provider_from_message(message),
            upstream_status: None,
            message: Some(message.to_string()),
            body_excerpt: excerpt_after_colon(message),
        };
    }

    if message.starts_with("failed to run ")
        || message.starts_with("failed to wait for ")
        || message.contains(" exited with ")
        || message.contains(" stream idle timeout ")
        || message.contains("capability token for subtask ")
    {
        return TurnFailure {
            kind: TurnFailureKind::LocalProcess,
            provider: provider_from_message(message),
            upstream_status: parse_upstream_status(message),
            message: Some(message.to_string()),
            body_excerpt: excerpt_after_colon(message),
        };
    }

    TurnFailure {
        kind: TurnFailureKind::Unknown,
        provider: provider_from_message(message),
        upstream_status: parse_upstream_status(message),
        message: Some(message.to_string()),
        body_excerpt: excerpt_after_colon(message),
    }
}

pub fn is_billing_failure_message(message: &str) -> bool {
    let lower = message.to_lowercase();
    lower.contains("payment required")
        || lower.contains("billing failure")
        || lower.contains("insufficient available balance")
        || lower.contains("insufficient balance")
}

pub fn is_quota_failure_message(message: &str) -> bool {
    let lower = message.to_lowercase();
    lower.contains("quota exhausted")
        || lower.contains("hit your limit")
        || lower.contains("usage limit")
        || lower.contains("rate limit exceeded")
        || lower.contains("rate_limit_exceeded")
        || lower.contains("too many requests")
        || lower.contains("api returned 429")
        || lower.contains("status 429")
        || lower.contains("status: 429")
        || lower.contains("error 429")
        || lower.contains("code 429")
        || lower.contains("http 429")
        || lower.contains("quota resets")
        || lower.contains("quota reset")
}

fn provider_from_message(message: &str) -> Option<String> {
    if let Some(rest) = message.strip_prefix("failed to run ") {
        return rest
            .split_once(':')
            .map(|(provider, _)| provider.to_string());
    }
    if let Some(rest) = message.strip_prefix("failed to wait for ") {
        return rest
            .split_once(':')
            .map(|(provider, _)| provider.to_string());
    }

    for provider in ["codex", "claude", "anthropic-api"] {
        if message.contains(provider) {
            return Some(provider.to_string());
        }
    }

    None
}

fn parse_upstream_status(message: &str) -> Option<u16> {
    if let Some(rest) = message.strip_prefix("API returned ") {
        let digits: String = rest.chars().take_while(|ch| ch.is_ascii_digit()).collect();
        return digits.parse().ok();
    }

    None
}

fn excerpt_after_colon(message: &str) -> Option<String> {
    let (_, tail) = message.split_once(':')?;
    let excerpt = tail.trim();
    if excerpt.is_empty() {
        return None;
    }

    let mut end = excerpt.len().min(MAX_FAILURE_BODY_EXCERPT_CHARS);
    while end > 0 && !excerpt.is_char_boundary(end) {
        end -= 1;
    }
    Some(excerpt[..end].to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_status_classification_does_not_depend_on_message_format() {
        for (status, message, expected) in [
            (
                401,
                "provider rejected credentials",
                TurnFailureKind::RequestRejected,
            ),
            (
                500,
                "API returned 401 in an earlier request",
                TurnFailureKind::Upstream,
            ),
            (402, "billing_error", TurnFailureKind::Billing),
            (429, "monthly spend cap reached", TurnFailureKind::Quota),
            (400, "insufficient balance", TurnFailureKind::Billing),
            (400, "quota exhausted", TurnFailureKind::Quota),
        ] {
            let failure = HarnessError::AgentHttpResponse {
                provider: "anthropic-api".to_string(),
                status,
                message: message.to_string(),
            }
            .turn_failure()
            .expect("typed HTTP failure");
            assert_eq!(failure.kind, expected);
            assert_eq!(failure.upstream_status, Some(status));
            assert_eq!(failure.provider.as_deref(), Some("anthropic-api"));
            assert_eq!(failure.message.as_deref(), Some(message));
        }
    }

    #[test]
    fn classify_streamed_quota_exit_as_quota_failure() {
        let failure = HarnessError::AgentExecution(
            "claude exited with exit status: 1: stderr=[] stdout_tail=[You've hit your limit · resets 3pm (Asia/Shanghai)\n]"
                .to_string(),
        )
        .turn_failure()
        .expect("turn failure");

        assert_eq!(failure.kind, TurnFailureKind::Quota);
        assert_eq!(failure.provider.as_deref(), Some("claude"));
    }

    #[test]
    fn classify_streamed_billing_exit_as_billing_failure() {
        let failure = HarnessError::AgentExecution(
            "codex exited with exit status: 1: payment required: insufficient available balance"
                .to_string(),
        )
        .turn_failure()
        .expect("turn failure");

        assert_eq!(failure.kind, TurnFailureKind::Billing);
        assert_eq!(failure.provider.as_deref(), Some("codex"));
    }

    #[test]
    fn do_not_treat_generic_rate_limit_mentions_as_quota_failures() {
        let failure = HarnessError::AgentExecution(
            "codex exited with exit status: 1: stdout_tail=[please add rate limit backoff before retrying this request]"
                .to_string(),
        )
        .turn_failure()
        .expect("turn failure");

        assert_eq!(failure.kind, TurnFailureKind::LocalProcess);
        assert_eq!(failure.provider.as_deref(), Some("codex"));
    }

    #[test]
    fn classify_agent_stream_stall_as_timeout_failure() {
        let Some(failure) =
            HarnessError::AgentExecution("Agent stream stalled: no output for 600s".to_string())
                .turn_failure()
        else {
            panic!("turn failure");
        };

        assert_eq!(failure.kind, TurnFailureKind::Timeout);
        assert_eq!(
            failure.message.as_deref(),
            Some("Agent stream stalled: no output for 600s")
        );
    }

    #[test]
    fn structured_upstream_failure_preserves_transport_classification() {
        let failure = HarnessError::Upstream(
            "codex structured error: provider temporarily unavailable".to_string(),
        )
        .turn_failure()
        .expect("turn failure");

        assert_eq!(failure.kind, TurnFailureKind::Upstream);
        assert_eq!(failure.provider.as_deref(), Some("codex"));
    }
}
