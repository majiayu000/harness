use crate::http::AppState;
use harness_core::{EventFilters, Grade};
use harness_observe::health::generate_health_report;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::sleep;

pub struct Scheduler {
    pub gc_interval: Duration,
    pub health_interval: Duration,
}

impl Scheduler {
    pub fn from_grade(grade: Grade) -> Self {
        Self {
            gc_interval: grade.recommended_gc_interval(),
            health_interval: Duration::from_secs(24 * 3600),
        }
    }

    pub fn start(self, state: Arc<AppState>) {
        let gc_state = state.clone();
        let gc_interval = self.gc_interval;
        tokio::spawn(async move {
            loop {
                sleep(gc_interval).await;
                tracing::info!("scheduler: triggering periodic GC run");
                crate::handlers::gc::gc_run(&gc_state, None).await;
            }
        });

        let health_state = state;
        let health_interval = self.health_interval;
        tokio::spawn(async move {
            loop {
                sleep(health_interval).await;
                // Query historical events before persisting the current scan to avoid the
                // just-persisted rule_check events inflating the quality stability score.
                let events = match health_state.events.query(&EventFilters::default()) {
                    Ok(e) => e,
                    Err(err) => {
                        tracing::warn!("scheduler: failed to query events: {err}");
                        continue;
                    }
                };
                let violations = {
                    let rules = health_state.rules.read().await;
                    rules.scan(&std::path::PathBuf::from(".")).await.unwrap_or_default()
                };
                let project_root = std::path::PathBuf::from(".");
                health_state.events.persist_rule_scan(&project_root, &violations);
                let report = generate_health_report(&events, &violations);
                tracing::info!(
                    grade = ?report.quality.grade,
                    score = report.quality.score,
                    violations = report.violation_summary.len(),
                    "scheduler: periodic health report"
                );
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use harness_core::Grade;

    #[test]
    fn from_grade_d_returns_1h_gc_interval() {
        let s = Scheduler::from_grade(Grade::D);
        assert_eq!(s.gc_interval, Duration::from_secs(3600));
        assert_eq!(s.health_interval, Duration::from_secs(24 * 3600));
    }

    #[test]
    fn from_grade_a_returns_7d_gc_interval() {
        let s = Scheduler::from_grade(Grade::A);
        assert_eq!(s.gc_interval, Duration::from_secs(7 * 24 * 3600));
        assert_eq!(s.health_interval, Duration::from_secs(24 * 3600));
    }

    #[test]
    fn from_grade_b_returns_3d_gc_interval() {
        let s = Scheduler::from_grade(Grade::B);
        assert_eq!(s.gc_interval, Duration::from_secs(3 * 24 * 3600));
    }

    #[test]
    fn from_grade_c_returns_1d_gc_interval() {
        let s = Scheduler::from_grade(Grade::C);
        assert_eq!(s.gc_interval, Duration::from_secs(24 * 3600));
    }
}
