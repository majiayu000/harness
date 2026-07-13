//! Per-channel delivery worker: bounded retry with exponential backoff
//! (B-006), per-attempt audit records (B-007/B-008), loop guard by
//! construction — this module has no path to the producer API (B-009).

use std::sync::Arc;
use std::time::Duration;

use harness_core::alert::AlertPayload;
use harness_core::config::alerting::AlertChannelConfig;
use harness_core::types::{ExternalSignal, Severity};
use harness_observe::event_store::EventStore;
use tokio::sync::mpsc;

use super::adapters::{deliver_once, AlertTransport};

const BACKOFF_CAP: Duration = Duration::from_secs(60);

/// Append one `alert_delivery` audit record. Channel URLs and tokens are
/// never serialized (B-015): only the channel *name* identifies the target.
pub(crate) fn audit(events: &EventStore, severity: Severity, detail: serde_json::Value) {
    let signal = ExternalSignal::new("alerting".to_string(), severity, detail);
    if let Err(error) = events.log_external_signal(&signal) {
        // Audit failure must not re-enter the alert pipeline (B-009);
        // error-level log is the terminal escalation here (U-29).
        tracing::error!(%error, "failed to append alert audit record");
    }
}

pub(crate) fn attempt_audit_record(
    channel: &AlertChannelConfig,
    payload: &AlertPayload,
    attempt: u32,
    outcome: &str,
    error: Option<&str>,
    duration_ms: u128,
) -> serde_json::Value {
    serde_json::json!({
        "kind": "alert_delivery",
        "channel": channel.name,
        "channel_kind": channel.kind,
        "attempt": attempt,
        "max_attempts": channel.max_attempts,
        "outcome": outcome,
        "error": error,
        "event_class": payload.event_class.as_str(),
        "dedup_key": payload.dedup_key,
        "duration_ms": duration_ms,
    })
}

/// Deliver `payload` to one channel with bounded retry. Returns whether the
/// delivery ultimately succeeded; every attempt is audited either way.
pub(crate) async fn deliver_with_retry(
    channel: &AlertChannelConfig,
    feishu_credentials: Option<&(String, String)>,
    transport: &dyn AlertTransport,
    events: &EventStore,
    payload: &AlertPayload,
) -> bool {
    for attempt in 1..=channel.max_attempts {
        let started = std::time::Instant::now();
        let result = deliver_once(channel, feishu_credentials, transport, payload).await;
        let duration_ms = started.elapsed().as_millis();
        match result {
            Ok(()) => {
                audit(
                    events,
                    Severity::Low,
                    attempt_audit_record(channel, payload, attempt, "success", None, duration_ms),
                );
                return true;
            }
            Err(error) => {
                let terminal = attempt == channel.max_attempts;
                audit(
                    events,
                    if terminal {
                        Severity::High
                    } else {
                        Severity::Medium
                    },
                    attempt_audit_record(
                        channel,
                        payload,
                        attempt,
                        if terminal { "exhausted" } else { "failure" },
                        Some(&error.to_string()),
                        duration_ms,
                    ),
                );
                if terminal {
                    tracing::error!(
                        channel = %channel.name,
                        event_class = payload.event_class.as_str(),
                        attempts = channel.max_attempts,
                        "alert delivery exhausted retries"
                    );
                    return false;
                }
                let backoff = Duration::from_millis(
                    channel
                        .backoff_base_ms
                        .saturating_mul(1u64 << (attempt - 1).min(16)),
                )
                .min(BACKOFF_CAP);
                tokio::time::sleep(backoff).await;
            }
        }
    }
    false
}

/// Run one channel worker: drain the queue until it closes (B-017: each
/// channel owns its queue; a stalled channel never blocks the others).
pub(crate) async fn run_channel_worker(
    channel: AlertChannelConfig,
    feishu_credentials: Option<(String, String)>,
    transport: Arc<dyn AlertTransport>,
    events: Arc<EventStore>,
    mut rx: mpsc::Receiver<AlertPayload>,
) {
    while let Some(payload) = rx.recv().await {
        deliver_with_retry(
            &channel,
            feishu_credentials.as_ref(),
            transport.as_ref(),
            &events,
            &payload,
        )
        .await;
    }
}

#[cfg(test)]
mod tests {
    use super::super::adapters::test_support::MockTransport;
    use super::*;
    use harness_core::alert::{AlertClass, AlertSeverity};
    use harness_core::config::alerting::AlertChannelKind;

    fn channel(max_attempts: u32) -> AlertChannelConfig {
        AlertChannelConfig {
            name: "ops".into(),
            kind: AlertChannelKind::Webhook,
            url: Some("https://example.invalid/hook".into()),
            receive_id: None,
            max_attempts,
            backoff_base_ms: 1,
        }
    }

    fn payload() -> AlertPayload {
        AlertPayload::new(
            AlertClass::TaskFailureExhausted,
            AlertSeverity::Error,
            "task t-1 exhausted",
            "task_failure_exhausted:t-1",
        )
    }

    async fn audit_records(events: &EventStore) -> Vec<serde_json::Value> {
        events
            .query_external_signals(None)
            .unwrap()
            .into_iter()
            .map(|s| s.payload)
            .collect()
    }

    #[tokio::test]
    async fn success_after_transient_failure_audits_each_attempt() {
        let tmp = tempfile::tempdir().unwrap();
        let events = EventStore::new(tmp.path()).await.unwrap();
        let transport = MockTransport::failing_first(1);

        let delivered =
            deliver_with_retry(&channel(3), None, &transport, &events, &payload()).await;
        assert!(delivered);
        assert_eq!(transport.call_count(), 2);

        let records = audit_records(&events).await;
        assert_eq!(records.len(), 2);
        assert_eq!(records[0]["outcome"], "failure");
        assert_eq!(records[0]["attempt"], 1);
        assert_eq!(records[1]["outcome"], "success");
        assert_eq!(records[1]["attempt"], 2);
        assert_eq!(records[1]["channel"], "ops");
    }

    #[tokio::test]
    async fn exhausted_delivery_audits_terminal_failure_and_stops() {
        let tmp = tempfile::tempdir().unwrap();
        let events = EventStore::new(tmp.path()).await.unwrap();
        let transport = MockTransport::always_failing();

        let delivered =
            deliver_with_retry(&channel(3), None, &transport, &events, &payload()).await;
        assert!(!delivered);
        assert_eq!(transport.call_count(), 3, "exactly max_attempts posts");

        let records = audit_records(&events).await;
        assert_eq!(records.len(), 3);
        assert_eq!(records[2]["outcome"], "exhausted");
    }

    /// B-015: audit records never contain the channel URL.
    #[tokio::test]
    async fn audit_records_do_not_leak_channel_url() {
        let tmp = tempfile::tempdir().unwrap();
        let events = EventStore::new(tmp.path()).await.unwrap();
        let transport = MockTransport::always_failing();

        deliver_with_retry(&channel(1), None, &transport, &events, &payload()).await;
        for record in audit_records(&events).await {
            let serialized = record.to_string();
            assert!(
                !serialized.contains("example.invalid"),
                "audit leaked channel url: {serialized}"
            );
        }
    }
}
