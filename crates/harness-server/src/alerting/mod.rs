//! External alerting dispatcher (GH-1582).
//!
//! Inert without config (B-001): `AlertHandle::disabled()` makes every
//! `raise` a no-op. When enabled, producers enqueue non-blocking (B-011)
//! into a router task that applies class filtering and dedup (B-003/B-010)
//! and fans out to independent per-channel workers (B-017).
//!
//! Loop guard (B-009): delivery, audit, and heartbeat code have no path to
//! `raise` — the only producer API is `AlertHandle`, which this module
//! never invokes internally.

pub mod adapters;
pub mod delivery;
pub mod heartbeat;
pub mod policy;
pub mod producers;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use harness_core::alert::AlertPayload;
use harness_core::config::alerting::AlertingConfig;
use harness_core::types::Severity;
use harness_observe::event_store::EventStore;
use tokio::sync::{mpsc, Mutex};

use adapters::AlertTransport;

struct QueuedAlert {
    payload: AlertPayload,
    cooldown_override: Option<Duration>,
}

/// Cheap cloneable producer handle. `disabled()` is a no-op sink.
#[derive(Clone)]
pub struct AlertHandle {
    inner: Option<Arc<AlertDispatcher>>,
}

pub struct AlertDispatcher {
    /// Producer side of the router queue. `shutdown` takes it out to close
    /// the channel; a `None` here means the dispatcher is shutting down.
    tx: std::sync::RwLock<Option<mpsc::Sender<QueuedAlert>>>,
    dropped: AtomicU64,
    shutdown_flush: Duration,
    router: Mutex<Option<tokio::task::JoinHandle<()>>>,
    heartbeat: Mutex<Option<tokio::task::JoinHandle<()>>>,
    events: Arc<EventStore>,
}

impl AlertHandle {
    pub fn disabled() -> Self {
        Self { inner: None }
    }

    pub fn is_enabled(&self) -> bool {
        self.inner.is_some()
    }

    /// Non-blocking enqueue (B-011). Drops with an error-level log and a
    /// counted audit trail when the queue is full.
    pub fn raise(&self, payload: AlertPayload) {
        self.raise_with_cooldown_override(payload, None);
    }

    /// Enqueue with a producer-owned dedup window (B-019).
    pub fn raise_with_cooldown_override(&self, payload: AlertPayload, cooldown: Option<Duration>) {
        let Some(dispatcher) = &self.inner else {
            return;
        };
        let queued = QueuedAlert {
            payload,
            cooldown_override: cooldown,
        };
        let send_result = {
            let guard = dispatcher.tx.read().unwrap_or_else(|e| e.into_inner());
            match guard.as_ref() {
                Some(tx) => tx.try_send(queued).map_err(|error| match error {
                    mpsc::error::TrySendError::Full(_) => "full",
                    mpsc::error::TrySendError::Closed(_) => "closed",
                }),
                None => Err("shutdown"),
            }
        };
        if let Err(reason) = send_result {
            let dropped_total = dispatcher.dropped.fetch_add(1, Ordering::Relaxed) + 1;
            tracing::error!(
                reason,
                dropped_total,
                "alert queue rejected payload; alert dropped"
            );
        }
    }

    /// Total alerts dropped at the producer boundary.
    pub fn dropped_total(&self) -> u64 {
        self.inner
            .as_ref()
            .map(|d| d.dropped.load(Ordering::Relaxed))
            .unwrap_or(0)
    }

    /// Graceful shutdown: close the queue and give workers a bounded flush
    /// window (B-018). Alerts still pending after the window are recorded
    /// as failed via an audit record.
    pub async fn shutdown(&self) {
        let Some(dispatcher) = &self.inner else {
            return;
        };
        if let Some(handle) = dispatcher.heartbeat.lock().await.take() {
            handle.abort();
        }
        let router = dispatcher.router.lock().await.take();
        // Dropping the producer sender closes the router queue; the router
        // drains what is already enqueued, then closes the channel workers,
        // which drain their own queues before exiting.
        dispatcher
            .tx
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        if let Some(handle) = router {
            match tokio::time::timeout(dispatcher.shutdown_flush, handle).await {
                Ok(_) => {}
                Err(_) => {
                    delivery::audit(
                        &dispatcher.events,
                        Severity::High,
                        serde_json::json!({
                            "kind": "alert_shutdown_flush_timeout",
                            "flush_window_secs": dispatcher.shutdown_flush.as_secs(),
                        }),
                    );
                    tracing::error!(
                        flush_window_secs = dispatcher.shutdown_flush.as_secs(),
                        "alert shutdown flush window elapsed; pending alerts abandoned"
                    );
                }
            }
        }
    }
}

/// Spawn the alerting subsystem. Returns a disabled handle when the config
/// is off (B-001); config validity is enforced earlier by
/// `AlertingConfig::validate()` at startup (B-002).
pub fn spawn_alerting(
    config: &AlertingConfig,
    feishu_credentials: Option<(String, String)>,
    events: Arc<EventStore>,
    transport: Arc<dyn AlertTransport>,
) -> AlertHandle {
    if !config.enabled {
        return AlertHandle::disabled();
    }

    let (tx, mut rx) = mpsc::channel::<QueuedAlert>(config.queue_capacity.max(1));

    // One bounded queue + worker per channel (B-017).
    let mut channel_txs: Vec<(String, mpsc::Sender<AlertPayload>)> = Vec::new();
    for channel in &config.channels {
        let (channel_tx, channel_rx) = mpsc::channel::<AlertPayload>(config.queue_capacity.max(1));
        channel_txs.push((channel.name.clone(), channel_tx));
        tokio::spawn(delivery::run_channel_worker(
            channel.clone(),
            feishu_credentials.clone(),
            transport.clone(),
            events.clone(),
            channel_rx,
        ));
    }

    let mut policy = policy::AlertPolicy::new(
        config.event_classes.iter().copied(),
        Duration::from_secs(config.dedup_cooldown_secs),
    );
    let router_events = events.clone();
    let router = tokio::spawn(async move {
        while let Some(queued) = rx.recv().await {
            let decision = policy.evaluate(
                &queued.payload,
                queued.cooldown_override,
                tokio::time::Instant::now(),
            );
            match decision {
                policy::PolicyDecision::FilteredClass => {}
                policy::PolicyDecision::Suppressed => {
                    tracing::debug!(
                        dedup_key = %queued.payload.dedup_key,
                        "alert suppressed by dedup cooldown"
                    );
                }
                policy::PolicyDecision::Deliver {
                    suppressed_in_window,
                } => {
                    if suppressed_in_window > 0 {
                        delivery::audit(
                            &router_events,
                            Severity::Low,
                            serde_json::json!({
                                "kind": "alert_dedup_window",
                                "dedup_key": queued.payload.dedup_key,
                                "suppressed_in_window": suppressed_in_window,
                            }),
                        );
                    }
                    for (name, channel_tx) in &channel_txs {
                        if let Err(error) = channel_tx.try_send(queued.payload.clone()) {
                            let reason = match error {
                                mpsc::error::TrySendError::Full(_) => "full",
                                mpsc::error::TrySendError::Closed(_) => "closed",
                            };
                            delivery::audit(
                                &router_events,
                                Severity::High,
                                serde_json::json!({
                                    "kind": "alert_channel_queue_drop",
                                    "channel": name,
                                    "reason": reason,
                                    "dedup_key": queued.payload.dedup_key,
                                }),
                            );
                            tracing::error!(
                                channel = %name,
                                reason,
                                "alert channel queue rejected payload"
                            );
                        }
                    }
                }
            }
        }
        // Queue closed: dropping channel_txs closes the workers, which
        // drain their remaining items before exiting.
    });

    let heartbeat_handle = heartbeat::spawn_heartbeat(config.heartbeat.clone(), transport);

    AlertHandle {
        inner: Some(Arc::new(AlertDispatcher {
            tx: std::sync::RwLock::new(Some(tx)),
            dropped: AtomicU64::new(0),
            shutdown_flush: Duration::from_secs(config.shutdown_flush_secs),
            router: Mutex::new(Some(router)),
            heartbeat: Mutex::new(heartbeat_handle),
            events,
        })),
    }
}

#[cfg(test)]
mod tests {
    use super::adapters::test_support::MockTransport;
    use super::*;
    use harness_core::alert::{AlertClass, AlertSeverity};
    use harness_core::config::alerting::{AlertChannelConfig, AlertChannelKind};

    fn test_config(channels: Vec<AlertChannelConfig>) -> AlertingConfig {
        AlertingConfig {
            enabled: true,
            event_classes: vec![
                AlertClass::TaskFailureExhausted,
                AlertClass::WorkflowBlocked,
            ],
            dedup_cooldown_secs: 0,
            queue_capacity: 8,
            shutdown_flush_secs: 5,
            channels,
            heartbeat: Default::default(),
        }
    }

    fn webhook_channel(name: &str) -> AlertChannelConfig {
        AlertChannelConfig {
            name: name.into(),
            kind: AlertChannelKind::Webhook,
            url: Some("https://example.invalid/hook".into()),
            receive_id: None,
            max_attempts: 2,
            backoff_base_ms: 1,
        }
    }

    fn payload(entity: &str) -> AlertPayload {
        AlertPayload::new(
            AlertClass::TaskFailureExhausted,
            AlertSeverity::Error,
            "test alert",
            AlertPayload::dedup_key_for(AlertClass::TaskFailureExhausted, entity),
        )
    }

    async fn wait_for_calls(transport: &MockTransport, at_least: usize) {
        for _ in 0..200 {
            if transport.call_count() >= at_least {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    /// B-001: disabled config yields an inert handle; raise is a no-op.
    #[tokio::test]
    async fn disabled_config_is_inert() {
        let tmp = tempfile::tempdir().unwrap();
        let events = Arc::new(EventStore::new(tmp.path()).await.unwrap());
        let transport = Arc::new(MockTransport::ok());
        let handle = spawn_alerting(
            &AlertingConfig::default(),
            None,
            events.clone(),
            transport.clone(),
        );
        assert!(!handle.is_enabled());
        handle.raise(payload("t-1"));
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(transport.call_count(), 0);
        assert!(events.query_external_signals(None).unwrap().is_empty());
    }

    /// B-009: an always-failing channel terminates with a failed audit
    /// chain and never feeds new alerts back into the pipeline.
    #[tokio::test]
    async fn loop_guard_failing_channel_no_re_raise() {
        let tmp = tempfile::tempdir().unwrap();
        let events = Arc::new(EventStore::new(tmp.path()).await.unwrap());
        let transport = Arc::new(MockTransport::always_failing());
        let handle = spawn_alerting(
            &test_config(vec![webhook_channel("ops")]),
            None,
            events.clone(),
            transport.clone(),
        );
        handle.raise(payload("t-loop"));
        wait_for_calls(&transport, 2).await;
        handle.shutdown().await;

        // Exactly max_attempts transport calls: nothing re-raised.
        assert_eq!(transport.call_count(), 2);
        let signals = events.query_external_signals(None).unwrap();
        let exhausted: Vec<_> = signals
            .iter()
            .filter(|s| s.payload["outcome"] == "exhausted")
            .collect();
        assert_eq!(exhausted.len(), 1);
        assert_eq!(handle.dropped_total(), 0);
    }

    /// B-011: producers never block; overflow is dropped and counted.
    #[tokio::test]
    async fn queue_full_drops_without_blocking() {
        let tmp = tempfile::tempdir().unwrap();
        let events = Arc::new(EventStore::new(tmp.path()).await.unwrap());
        // Stalled channel: worker never completes, so the router queue and
        // channel queue back up while the producer keeps try_send-ing.
        let transport = Arc::new(MockTransport::ok());
        let mut config = test_config(vec![webhook_channel("ops")]);
        config.queue_capacity = 1;
        let handle = spawn_alerting(&config, None, events, transport);
        for i in 0..64 {
            handle.raise(payload(&format!("t-{i}")));
        }
        // Non-blocking by construction (raise is sync try_send); some
        // overflow must have been counted at this capacity.
        assert!(handle.dropped_total() > 0);
        handle.shutdown().await;
    }

    /// B-017: a failing channel does not stop a healthy channel.
    #[tokio::test]
    async fn multi_channel_independent_delivery() {
        let tmp = tempfile::tempdir().unwrap();
        let events = Arc::new(EventStore::new(tmp.path()).await.unwrap());
        let transport = Arc::new(MockTransport::failing_first(2));
        // Channel "bad" burns the two failures (max_attempts 2), channel
        // "good" then succeeds — order between workers is not guaranteed,
        // so assert on audit outcomes per channel instead.
        let handle = spawn_alerting(
            &test_config(vec![webhook_channel("bad"), webhook_channel("good")]),
            None,
            events.clone(),
            transport.clone(),
        );
        handle.raise(payload("t-mc"));
        wait_for_calls(&transport, 3).await;
        handle.shutdown().await;

        let signals = events.query_external_signals(None).unwrap();
        let successes: Vec<_> = signals
            .iter()
            .filter(|s| s.payload["outcome"] == "success")
            .collect();
        assert!(
            !successes.is_empty(),
            "at least one channel must deliver despite the other failing"
        );
    }

    /// B-018: shutdown flushes queued alerts within the window.
    #[tokio::test]
    async fn shutdown_flushes_pending_alerts() {
        let tmp = tempfile::tempdir().unwrap();
        let events = Arc::new(EventStore::new(tmp.path()).await.unwrap());
        let transport = Arc::new(MockTransport::ok());
        let handle = spawn_alerting(
            &test_config(vec![webhook_channel("ops")]),
            None,
            events.clone(),
            transport.clone(),
        );
        for i in 0..4 {
            handle.raise(payload(&format!("t-flush-{i}")));
        }
        handle.shutdown().await;
        assert_eq!(
            transport.call_count(),
            4,
            "queued alerts flushed on shutdown"
        );
        // Raising after shutdown is a counted no-op, not a panic.
        handle.raise(payload("t-late"));
        assert!(handle.dropped_total() >= 1);
    }
}
