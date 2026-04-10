use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Start a background task that sets the returned flag to `true` whenever
/// available system memory falls below `threshold_mb` MB, and `false` when
/// it recovers.  Sampling occurs every `poll_secs` seconds (clamped to ≥ 1).
///
/// The monitor runs for the caller's lifetime; dropping the returned
/// `Arc<AtomicBool>` does **not** stop the task — the task stops when the
/// tokio runtime shuts down.
///
/// This function is a no-op until a tokio runtime is active (it calls
/// `tokio::spawn` internally).
pub fn start(threshold_mb: u64, poll_secs: u64) -> Arc<AtomicBool> {
    start_with_sampler(threshold_mb, poll_secs, sample_available_mb)
}

/// Like [`start`] but accepts a custom sampler that returns available memory
/// in megabytes.  Intended for unit tests that must not call real system APIs.
pub fn start_with_sampler<F>(threshold_mb: u64, poll_secs: u64, sampler: F) -> Arc<AtomicBool>
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
