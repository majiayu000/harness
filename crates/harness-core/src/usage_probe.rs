use crate::types::{Decision, Event, SessionId};
use serde::Serialize;
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageProbeSurface {
    ThreadRpc,
    TurnRpc,
    ThreadManager,
    TaskDb,
    TaskExecutor,
    TaskRunner,
    HarnessEval,
    ReviewStore,
}

impl UsageProbeSurface {
    pub const ALL: [Self; 8] = [
        Self::ThreadRpc,
        Self::TurnRpc,
        Self::ThreadManager,
        Self::TaskDb,
        Self::TaskExecutor,
        Self::TaskRunner,
        Self::HarnessEval,
        Self::ReviewStore,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ThreadRpc => "thread_rpc",
            Self::TurnRpc => "turn_rpc",
            Self::ThreadManager => "thread_manager",
            Self::TaskDb => "task_db",
            Self::TaskExecutor => "task_executor",
            Self::TaskRunner => "task_runner",
            Self::HarnessEval => "harness_eval",
            Self::ReviewStore => "review_store",
        }
    }

    fn counter(self) -> &'static AtomicU64 {
        match self {
            Self::ThreadRpc => &THREAD_RPC_COUNT,
            Self::TurnRpc => &TURN_RPC_COUNT,
            Self::ThreadManager => &THREAD_MANAGER_COUNT,
            Self::TaskDb => &TASK_DB_COUNT,
            Self::TaskExecutor => &TASK_EXECUTOR_COUNT,
            Self::TaskRunner => &TASK_RUNNER_COUNT,
            Self::HarnessEval => &HARNESS_EVAL_COUNT,
            Self::ReviewStore => &REVIEW_STORE_COUNT,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UsageProbeCount {
    pub surface: &'static str,
    pub count: u64,
}

static THREAD_RPC_COUNT: AtomicU64 = AtomicU64::new(0);
static TURN_RPC_COUNT: AtomicU64 = AtomicU64::new(0);
static THREAD_MANAGER_COUNT: AtomicU64 = AtomicU64::new(0);
static TASK_DB_COUNT: AtomicU64 = AtomicU64::new(0);
static TASK_EXECUTOR_COUNT: AtomicU64 = AtomicU64::new(0);
static TASK_RUNNER_COUNT: AtomicU64 = AtomicU64::new(0);
static HARNESS_EVAL_COUNT: AtomicU64 = AtomicU64::new(0);
static REVIEW_STORE_COUNT: AtomicU64 = AtomicU64::new(0);

pub fn record_usage(surface: UsageProbeSurface) {
    surface.counter().fetch_add(1, Ordering::Relaxed);
}

pub fn snapshot() -> Vec<UsageProbeCount> {
    UsageProbeSurface::ALL
        .into_iter()
        .map(|surface| UsageProbeCount {
            surface: surface.as_str(),
            count: surface.counter().load(Ordering::Relaxed),
        })
        .collect()
}

pub fn build_probe_report_event() -> anyhow::Result<Event> {
    let mut event = Event::new(
        SessionId::new(),
        "probe_report",
        "usage_probe",
        Decision::Complete,
    );
    event.detail = Some(serde_json::to_string(&snapshot())?);
    Ok(event)
}

#[doc(hidden)]
pub fn reset_for_tests() {
    for surface in UsageProbeSurface::ALL {
        surface.counter().store(0, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn count_for(surface: &str) -> u64 {
        snapshot()
            .into_iter()
            .find(|entry| entry.surface == surface)
            .map(|entry| entry.count)
            .unwrap_or(0)
    }

    #[test]
    fn snapshot_includes_recorded_surface_counts() {
        let thread_before = count_for("thread_rpc");
        let review_before = count_for("review_store");

        record_usage(UsageProbeSurface::ThreadRpc);
        record_usage(UsageProbeSurface::ThreadRpc);
        record_usage(UsageProbeSurface::ReviewStore);

        assert!(count_for("thread_rpc") >= thread_before + 2);
        assert!(count_for("review_store") >= review_before + 1);
    }

    #[test]
    fn probe_report_event_uses_queryable_detail() -> anyhow::Result<()> {
        let before = count_for("task_runner");
        record_usage(UsageProbeSurface::TaskRunner);

        let event = build_probe_report_event()?;

        assert_eq!(event.hook, "probe_report");
        assert_eq!(event.tool, "usage_probe");
        assert_eq!(event.decision, Decision::Complete);
        let detail = event.detail.expect("detail");
        let counts: Vec<serde_json::Value> = serde_json::from_str(&detail)?;
        let task_runner_count = counts
            .iter()
            .find(|entry| entry["surface"] == "task_runner")
            .and_then(|entry| entry["count"].as_u64())
            .unwrap_or(0);
        assert!(task_runner_count >= before + 1);
        assert!(
            event.content.is_none(),
            "queryable summary must be in detail"
        );
        Ok(())
    }
}
