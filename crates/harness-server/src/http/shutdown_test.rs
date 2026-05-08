use super::{
    axum_graceful_shutdown_signal, drain_or_force, wait_for_first_signal_then_drain_or_force,
    ShutdownReason,
};
use harness_core::config::shutdown::ShutdownConfig;
use std::time::Duration;
use tokio::time::Instant;

/// drain_or_force returns DrainTimeout when no user-force signal arrives
/// before the drain deadline expires. Uses real time with a tiny deadline
/// so the test stays fast.
#[tokio::test]
async fn drain_timeout_elapses_without_user_force() {
    let started = Instant::now();
    let reason = drain_or_force(1, 5, std::future::pending::<()>()).await;
    assert_eq!(reason, ShutdownReason::DrainTimeout);
    assert!(started.elapsed() >= Duration::from_secs(1));
}

/// drain_or_force returns UserForced when the second signal arrives
/// before the drain deadline.
#[tokio::test]
async fn second_signal_forces_before_drain_deadline() {
    let user_force = async {
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    let started = Instant::now();
    let reason = drain_or_force(60, 5, user_force).await;
    assert_eq!(reason, ShutdownReason::UserForced);
    // Must finish well before the 60s drain deadline.
    assert!(started.elapsed() < Duration::from_secs(5));
}

/// Progress-log interval of 0 does not panic — the implementation clamps
/// to 1s so the interval timer never spins.
#[tokio::test]
async fn progress_log_secs_zero_does_not_panic() {
    let reason = drain_or_force(1, 0, std::future::pending::<()>()).await;
    assert_eq!(reason, ShutdownReason::DrainTimeout);
}

/// When the user-force future is already ready, biased select returns
/// UserForced immediately without waiting for any timer.
#[tokio::test]
async fn immediately_ready_user_force_wins() {
    let started = Instant::now();
    let reason = drain_or_force(60, 5, async {}).await;
    assert_eq!(reason, ShutdownReason::UserForced);
    assert!(started.elapsed() < Duration::from_millis(500));
}

#[tokio::test]
async fn drain_deadline_waits_for_first_shutdown_signal() {
    let (first_signal_tx, first_signal_rx) = tokio::sync::watch::channel(false);
    let cfg = ShutdownConfig {
        drain_timeout_secs: 0,
        force_grace_secs: 60,
        progress_log_secs: 5,
    };

    let mut shutdown = Box::pin(wait_for_first_signal_then_drain_or_force(
        first_signal_rx,
        cfg,
        std::future::pending::<()>(),
    ));

    assert!(
        tokio::time::timeout(Duration::from_millis(50), &mut shutdown)
            .await
            .is_err(),
        "drain deadline must not begin before the first shutdown signal"
    );

    first_signal_tx.send(true).unwrap();
    let reason = tokio::time::timeout(Duration::from_millis(50), shutdown)
        .await
        .expect("drain deadline should start after the first shutdown signal");
    assert_eq!(reason, Some(ShutdownReason::DrainTimeout));
}

#[tokio::test]
async fn axum_graceful_signal_resolves_on_first_signal_before_drain_deadline() {
    let (first_signal_tx, first_signal_rx_serve) = tokio::sync::watch::channel(false);
    let first_signal_rx_force = first_signal_tx.subscribe();
    let cfg = ShutdownConfig {
        drain_timeout_secs: 60,
        force_grace_secs: 60,
        progress_log_secs: 5,
    };

    let mut axum_signal = Box::pin(axum_graceful_shutdown_signal(
        first_signal_rx_serve,
        cfg.clone(),
    ));
    let mut force_watcher = Box::pin(wait_for_first_signal_then_drain_or_force(
        first_signal_rx_force,
        cfg,
        std::future::pending::<()>(),
    ));

    assert!(
        tokio::time::timeout(Duration::from_millis(50), &mut axum_signal)
            .await
            .is_err(),
        "Axum shutdown signal must wait until the first shutdown signal"
    );

    first_signal_tx.send(true).unwrap();
    tokio::time::timeout(Duration::from_millis(50), axum_signal)
        .await
        .expect("Axum must begin graceful shutdown immediately on first signal");

    assert!(
        tokio::time::timeout(Duration::from_millis(50), &mut force_watcher)
            .await
            .is_err(),
        "force watcher must keep draining instead of delaying Axum's graceful signal"
    );
}
