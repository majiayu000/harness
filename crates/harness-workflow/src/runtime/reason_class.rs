//! Stop-reason classification for workflow runtime auto-recovery (GH-1584).
//!
//! Every stopped (`blocked` / `failed`) workflow instance carries an optional
//! structured `stop_reason_code` plus an optional `error_kind`. This module is
//! the single classifier (B-001) that maps that pair to a
//! [`StopReasonClass`]: `transient` reasons may be auto-rechecked by the
//! opt-in recovery scheduler, `terminal` reasons always wait for a human.
//!
//! The classifier fails closed (B-002, B-014): a missing, empty, unknown, or
//! unclassifiable stop reason is `Terminal` and is never auto-recovered.
//!
//! Naming note: wire codes (`stop_reason_code` values and serialized
//! `error_kind` values) are snake_case; in Rust the `error_kind` values are
//! the corresponding [`ActivityErrorKind`] PascalCase variants (for example
//! `retryable` <-> `ActivityErrorKind::Retryable`). `classify_stop` takes the
//! enum; the snake_case forms appear only in persisted JSON and audit events.

use crate::runtime::model::ActivityErrorKind;
use serde::{Deserialize, Serialize};

/// Classification of a structured stop reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReasonClass {
    /// Machine-recheckable: the opt-in auto-recovery policy may retry it.
    Transient,
    /// Operator-only: the manual unblock/retry API is the only recovery path.
    Terminal,
}

impl StopReasonClass {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Transient => "transient",
            Self::Terminal => "terminal",
        }
    }
}

/// Stop reason code for GitHub rate limiting (transient).
pub const STOP_REASON_RATE_LIMITED: &str = "rate_limited";
/// Stop reason code for a CI backend outage (transient).
pub const STOP_REASON_CI_BACKEND_UNAVAILABLE: &str = "ci_backend_unavailable";
/// Stop reason code for merge-base drift resolved by a rebase (transient).
pub const STOP_REASON_MERGE_BASE_DRIFT: &str = "merge_base_drift";
/// Stop reason code for a runtime circuit-breaker cooldown (transient).
pub const STOP_REASON_CIRCUIT_BREAKER_COOLDOWN: &str = "circuit_breaker_cooldown";
/// Stop reason code for a transient runtime transcript store failure.
pub const STOP_REASON_RUNTIME_TRANSCRIPT_STORE_UNAVAILABLE: &str =
    "runtime_transcript_store_unavailable";
/// Stop reason code for confirmed runtime transcript loss or corruption.
pub const STOP_REASON_RUNTIME_TRANSCRIPT_LOST: &str = "runtime_transcript_lost";
/// Stop reason code for a block that needs maintainer/operator input (terminal).
pub const STOP_REASON_MAINTAINER_INPUT_REQUIRED: &str = "maintainer_input_required";
/// Stop reason code for structurally invalid agent output (terminal).
pub const STOP_REASON_INVALID_AGENT_OUTPUT: &str = "invalid_agent_output";

/// The initial transient stop-reason code set. Expanding this table is a
/// spec/config change, never a runtime inference.
pub const TRANSIENT_STOP_REASON_CODES: &[&str] = &[
    STOP_REASON_RATE_LIMITED,
    STOP_REASON_CI_BACKEND_UNAVAILABLE,
    STOP_REASON_MERGE_BASE_DRIFT,
    STOP_REASON_CIRCUIT_BREAKER_COOLDOWN,
    STOP_REASON_RUNTIME_TRANSCRIPT_STORE_UNAVAILABLE,
];

/// Single classifier used by the scheduler, API serialization, and audit
/// events (B-001).
///
/// Precedence (fail closed, B-002 / B-012):
/// 1. A non-retryable `error_kind` (`fatal`, `configuration`, `unknown`) is
///    `Terminal` even if a transient `stop_reason_code` was forged next to it.
/// 2. A present `stop_reason_code` is matched against the transient table;
///    anything else — including empty or unknown codes — is `Terminal`.
/// 3. With no code, a retryable `error_kind` (`retryable`, `timeout`,
///    `spawn_failure`, `external_dependency`) is `Transient`.
/// 4. Both absent (legacy rows, B-014) is `Terminal`.
pub fn classify_stop(
    stop_reason_code: Option<&str>,
    error_kind: Option<ActivityErrorKind>,
) -> StopReasonClass {
    if matches!(
        error_kind,
        Some(
            ActivityErrorKind::Fatal
                | ActivityErrorKind::Configuration
                | ActivityErrorKind::Unknown
        )
    ) {
        return StopReasonClass::Terminal;
    }
    if let Some(code) = stop_reason_code {
        let code = code.trim();
        if TRANSIENT_STOP_REASON_CODES.contains(&code) {
            return StopReasonClass::Transient;
        }
        return StopReasonClass::Terminal;
    }
    match error_kind {
        Some(
            ActivityErrorKind::Retryable
            | ActivityErrorKind::Timeout
            | ActivityErrorKind::SpawnFailure
            | ActivityErrorKind::ExternalDependency,
        ) => StopReasonClass::Transient,
        _ => StopReasonClass::Terminal,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reason_class_transient_codes_classify_transient() {
        for code in TRANSIENT_STOP_REASON_CODES {
            assert_eq!(
                classify_stop(Some(code), None),
                StopReasonClass::Transient,
                "code {code} must be transient"
            );
        }
    }

    #[test]
    fn reason_class_terminal_codes_classify_terminal() {
        for code in [
            STOP_REASON_MAINTAINER_INPUT_REQUIRED,
            STOP_REASON_INVALID_AGENT_OUTPUT,
            "some_future_code",
        ] {
            assert_eq!(classify_stop(Some(code), None), StopReasonClass::Terminal);
        }
    }

    #[test]
    fn reason_class_fails_closed_on_missing_empty_or_garbage_codes() {
        // B-002: missing, empty, unknown, or unclassifiable -> Terminal.
        assert_eq!(classify_stop(None, None), StopReasonClass::Terminal);
        assert_eq!(classify_stop(Some(""), None), StopReasonClass::Terminal);
        assert_eq!(classify_stop(Some("   "), None), StopReasonClass::Terminal);
        assert_eq!(
            classify_stop(Some("RATE_LIMITED"), None),
            StopReasonClass::Terminal,
            "codes are case-sensitive snake_case wire values"
        );
        assert_eq!(
            classify_stop(Some("garbage \u{1F980}"), None),
            StopReasonClass::Terminal
        );
    }

    #[test]
    fn reason_class_trims_whitespace_around_known_codes() {
        assert_eq!(
            classify_stop(Some(" rate_limited "), None),
            StopReasonClass::Transient
        );
    }

    #[test]
    fn reason_class_error_kind_table_matches_spec() {
        // B-001: every error_kind maps to exactly one class.
        let table = [
            (ActivityErrorKind::Retryable, StopReasonClass::Transient),
            (ActivityErrorKind::Timeout, StopReasonClass::Transient),
            (ActivityErrorKind::SpawnFailure, StopReasonClass::Transient),
            (
                ActivityErrorKind::ExternalDependency,
                StopReasonClass::Transient,
            ),
            (ActivityErrorKind::Fatal, StopReasonClass::Terminal),
            (ActivityErrorKind::Configuration, StopReasonClass::Terminal),
            (ActivityErrorKind::Unknown, StopReasonClass::Terminal),
        ];
        for (kind, expected) in table {
            assert_eq!(classify_stop(None, Some(kind)), expected, "{kind:?}");
        }
    }

    #[test]
    fn reason_class_terminal_error_kind_beats_forged_transient_code() {
        // B-012: a forged transient code never overrides a fatal failure.
        for kind in [
            ActivityErrorKind::Fatal,
            ActivityErrorKind::Configuration,
            ActivityErrorKind::Unknown,
        ] {
            assert_eq!(
                classify_stop(Some(STOP_REASON_RATE_LIMITED), Some(kind)),
                StopReasonClass::Terminal
            );
        }
    }

    #[test]
    fn reason_class_transient_code_with_retryable_error_kind_is_transient() {
        assert_eq!(
            classify_stop(
                Some(STOP_REASON_CI_BACKEND_UNAVAILABLE),
                Some(ActivityErrorKind::Timeout)
            ),
            StopReasonClass::Transient
        );
    }

    #[test]
    fn reason_class_unknown_code_with_retryable_error_kind_fails_closed() {
        // An explicit-but-unknown code is unclassifiable evidence: Terminal.
        assert_eq!(
            classify_stop(Some("mystery"), Some(ActivityErrorKind::Retryable)),
            StopReasonClass::Terminal
        );
    }

    #[test]
    fn reason_class_serializes_snake_case() {
        assert_eq!(
            serde_json::to_value(StopReasonClass::Transient).unwrap(),
            serde_json::json!("transient")
        );
        assert_eq!(
            serde_json::to_value(StopReasonClass::Terminal).unwrap(),
            serde_json::json!("terminal")
        );
    }
}
