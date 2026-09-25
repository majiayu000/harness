use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::http::background::LoopHandle;

/// Starts the monitor and reports liveness into the loop-health registry so the
/// operator monitor flags the monitor if it dies (GH-1981). The first sample
/// is taken immediately, so the loop is never falsely stale at startup.
pub(crate) fn start_registered(
    handle: LoopHandle,
    threshold_mb: u64,
    poll_secs: u64,
) -> Arc<AtomicBool> {
    start_with_sampler_and_health(
        threshold_mb,
        poll_secs.max(1),
        Some(handle),
        sample_available_mb,
    )
}

/// Accepts a custom sampler that returns available memory
/// in megabytes.  Intended for unit tests that must not call real system APIs.
#[cfg(test)]
pub fn start_with_sampler<F>(threshold_mb: u64, poll_secs: u64, sampler: F) -> Arc<AtomicBool>
where
    F: Fn() -> u64 + Send + 'static,
{
    start_with_sampler_and_health(threshold_mb, poll_secs, None, sampler)
}

fn start_with_sampler_and_health<F>(
    threshold_mb: u64,
    poll_secs: u64,
    health: Option<LoopHandle>,
    sampler: F,
) -> Arc<AtomicBool>
where
    F: Fn() -> u64 + Send + 'static,
{
    let flag = Arc::new(AtomicBool::new(false));
    let flag_clone = flag.clone();
    let interval = Duration::from_secs(poll_secs.max(1));

    tokio::spawn(async move {
        loop {
            let available_mb = sampler();
            let under_pressure = available_mb < threshold_mb;
            flag_clone.store(under_pressure, Ordering::Relaxed);
            if under_pressure {
                tracing::warn!(
                    available_mb,
                    threshold_mb,
                    "memory pressure: available memory below threshold; new tasks will be rejected"
                );
            }
            if let Some(handle) = &health {
                handle.tick_ok();
            }
            tokio::time::sleep(interval).await;
        }
    });

    flag
}

/// Sample the system's available memory and return it in megabytes.
fn sample_available_mb() -> u64 {
    let mut sys = sysinfo::System::new();
    sys.refresh_memory();
    sys.available_memory() / (1024 * 1024)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU64;
    use tokio::time::{sleep, Duration};

    #[tokio::test]
    async fn monitor_sets_flag_when_below_threshold() {
        // Simulates 256 MB available with a 512 MB threshold → pressure.
        let flag = start_with_sampler(512, 1, || 256);
        sleep(Duration::from_millis(1100)).await;
        assert!(
            flag.load(Ordering::Relaxed),
            "flag should be true under pressure"
        );
    }

    #[tokio::test]
    async fn monitor_clears_flag_when_above_threshold() {
        // Counter starts high (above threshold), then drops below.
        let counter = Arc::new(AtomicU64::new(1024));
        let counter_clone = counter.clone();
        let flag = start_with_sampler(512, 1, move || counter_clone.load(Ordering::Relaxed));

        // First poll: 1024 MB available → no pressure.
        sleep(Duration::from_millis(1100)).await;
        assert!(
            !flag.load(Ordering::Relaxed),
            "flag should be false when memory is sufficient"
        );

        // Drop available memory below threshold.
        counter.store(256, Ordering::Relaxed);
        sleep(Duration::from_millis(1100)).await;
        assert!(
            flag.load(Ordering::Relaxed),
            "flag should be true after memory drops"
        );

        // Recover memory.
        counter.store(800, Ordering::Relaxed);
        sleep(Duration::from_millis(1100)).await;
        assert!(
            !flag.load(Ordering::Relaxed),
            "flag should clear when memory recovers"
        );
    }
}
