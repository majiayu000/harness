//! Dead-man-switch heartbeat (B-016): pings a monitoring URL on an
//! interval. Failures log at warn and never raise alerts (no self-alert).

use std::sync::Arc;
use std::time::Duration;

use harness_core::config::alerting::AlertingHeartbeatConfig;

use super::adapters::AlertTransport;

pub(crate) fn spawn_heartbeat(
    config: AlertingHeartbeatConfig,
    transport: Arc<dyn AlertTransport>,
) -> Option<tokio::task::JoinHandle<()>> {
    if !config.enabled {
        return None;
    }
    let url = config.url.clone()?;
    let interval = Duration::from_secs(config.interval_secs.max(1));
    Some(tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            let body = serde_json::json!({
                "source": "harness",
                "kind": "heartbeat",
                "ts": chrono::Utc::now().to_rfc3339(),
            });
            if let Err(error) = transport.post_json(&url, None, &body).await {
                // Warn only: the heartbeat must never feed the alert
                // pipeline (B-016); its silence IS the signal.
                tracing::warn!(%error, "alert heartbeat ping failed");
            }
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::super::adapters::test_support::MockTransport;
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn pings_on_interval_via_transport() {
        let transport = Arc::new(MockTransport::ok());
        let config = AlertingHeartbeatConfig {
            enabled: true,
            url: Some("https://ping.example.invalid/hb".into()),
            interval_secs: 60,
        };
        let handle = spawn_heartbeat(config, transport.clone()).expect("spawned");

        // First tick fires immediately; step through further intervals,
        // yielding between advances so the spawned task can run.
        for _ in 0..4 {
            tokio::task::yield_now().await;
            tokio::time::advance(Duration::from_secs(61)).await;
        }
        tokio::task::yield_now().await;
        assert!(
            transport.call_count() >= 2,
            "expected at least 2 pings, got {}",
            transport.call_count()
        );
        handle.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn failures_do_not_stop_the_loop() {
        let transport = Arc::new(MockTransport::always_failing());
        let config = AlertingHeartbeatConfig {
            enabled: true,
            url: Some("https://ping.example.invalid/hb".into()),
            interval_secs: 30,
        };
        let handle = spawn_heartbeat(config, transport.clone()).expect("spawned");
        for _ in 0..4 {
            tokio::task::yield_now().await;
            tokio::time::advance(Duration::from_secs(31)).await;
        }
        tokio::task::yield_now().await;
        assert!(
            transport.call_count() >= 3,
            "loop must continue after failures, got {}",
            transport.call_count()
        );
        handle.abort();
    }

    #[tokio::test]
    async fn disabled_heartbeat_spawns_nothing() {
        let transport = Arc::new(MockTransport::ok());
        assert!(spawn_heartbeat(AlertingHeartbeatConfig::default(), transport).is_none());
    }
}
