use super::{drain_or_force, ShutdownReason};
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
