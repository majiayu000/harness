//! Alert producers (SP1582-T006): payload builders used at the wiring
//! sites plus the notify-drop watcher.
//!
//! `notify.rs` stays free of any dispatcher dependency: the watcher here
//! polls the public drop counter and raises when it advances.

use std::time::Duration;

use harness_core::alert::{AlertClass, AlertPayload, AlertSeverity, AlertSubject};

use super::AlertHandle;

const NOTIFY_DROP_POLL_SECS: u64 = 30;

pub fn task_failure_exhausted(
    task_id: &str,
    issue: Option<&str>,
    repo: Option<&str>,
    attempts: u32,
    detail: &str,
) -> AlertPayload {
    AlertPayload::new(
        AlertClass::TaskFailureExhausted,
        AlertSeverity::Error,
        format!("task {task_id} exhausted {attempts} retries: {detail}"),
        AlertPayload::dedup_key_for(AlertClass::TaskFailureExhausted, task_id),
    )
    .with_subject(AlertSubject {
        task_id: Some(task_id.to_string()),
        issue: issue.map(str::to_string),
        repo: repo.map(str::to_string),
        ..AlertSubject::default()
    })
}

pub fn workflow_stopped(
    class: AlertClass,
    workflow_id: &str,
    issue: Option<&str>,
    repo: Option<&str>,
    reason: &str,
) -> AlertPayload {
    debug_assert!(matches!(
        class,
        AlertClass::WorkflowBlocked | AlertClass::WorkflowFailed
    ));
    AlertPayload::new(
        class,
        AlertSeverity::Error,
        format!("workflow {workflow_id} {}: {reason}", class.as_str()),
        AlertPayload::dedup_key_for(class, workflow_id),
    )
    .with_subject(AlertSubject {
        workflow_id: Some(workflow_id.to_string()),
        issue: issue.map(str::to_string),
        repo: repo.map(str::to_string),
        ..AlertSubject::default()
    })
}

pub fn circuit_breaker_open(scope: &str, detail: &str) -> AlertPayload {
    AlertPayload::new(
        AlertClass::CircuitBreakerOpen,
        AlertSeverity::Error,
        format!("circuit breaker opened for {scope}: {detail}"),
        AlertPayload::dedup_key_for(AlertClass::CircuitBreakerOpen, scope),
    )
    .with_subject(AlertSubject {
        entity: Some(scope.to_string()),
        ..AlertSubject::default()
    })
}

pub fn reconciliation_anomaly(entity: &str, detail: &str) -> AlertPayload {
    AlertPayload::new(
        AlertClass::ReconciliationAnomaly,
        AlertSeverity::Warning,
        format!("reconciliation anomaly on {entity}: {detail}"),
        AlertPayload::dedup_key_for(AlertClass::ReconciliationAnomaly, entity),
    )
    .with_subject(AlertSubject {
        entity: Some(entity.to_string()),
        ..AlertSubject::default()
    })
}

/// `ready_to_merge` aging (B-019). Raise with
/// [`AlertHandle::raise_with_cooldown_override`] using
/// `ready_to_merge_alert_ttl_secs` so the dispatcher's dedup enforces the
/// once-per-TTL bound without reconciliation persisting alert state.
pub fn ready_to_merge_aging(
    pr_url: &str,
    age_secs: u64,
    workflow_id: Option<&str>,
) -> AlertPayload {
    AlertPayload::new(
        AlertClass::ReadyToMergeAging,
        AlertSeverity::Warning,
        format!("PR ready_to_merge for {age_secs}s awaiting merge: {pr_url}"),
        AlertPayload::dedup_key_for(AlertClass::ReadyToMergeAging, pr_url),
    )
    .with_subject(AlertSubject {
        pr_url: Some(pr_url.to_string()),
        workflow_id: workflow_id.map(str::to_string),
        ..AlertSubject::default()
    })
}

fn notify_channel_drop(delta: u64, total: u64) -> AlertPayload {
    AlertPayload::new(
        AlertClass::NotifyChannelDrop,
        AlertSeverity::Warning,
        format!("dashboard notification channel dropped {delta} messages ({total} total)"),
        AlertPayload::dedup_key_for(AlertClass::NotifyChannelDrop, "notify"),
    )
}

/// Watch the `notify` drop counter and raise when it advances. Owned by the
/// alerting module; rate-limited by the dispatcher dedup window.
pub fn spawn_notify_drop_watcher(handle: AlertHandle) -> Option<tokio::task::JoinHandle<()>> {
    if !handle.is_enabled() {
        return None;
    }
    Some(tokio::spawn(async move {
        let mut last_seen = crate::notify::dropped_notification_count();
        let mut ticker = tokio::time::interval(Duration::from_secs(NOTIFY_DROP_POLL_SECS));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            let current = crate::notify::dropped_notification_count();
            if current > last_seen {
                handle.raise(notify_channel_drop(current - last_seen, current));
                last_seen = current;
            }
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_builders_carry_entity_refs() {
        let task = task_failure_exhausted("t-1", Some("42"), Some("o/r"), 3, "stalled");
        assert_eq!(task.subject.task_id.as_deref(), Some("t-1"));
        assert_eq!(task.subject.issue.as_deref(), Some("42"));
        assert_eq!(task.subject.repo.as_deref(), Some("o/r"));
        assert_eq!(task.dedup_key, "task_failure_exhausted:t-1");

        let wf = workflow_stopped(
            AlertClass::WorkflowBlocked,
            "wf-9",
            None,
            Some("o/r"),
            "rate limited",
        );
        assert_eq!(wf.subject.workflow_id.as_deref(), Some("wf-9"));
        assert_eq!(wf.dedup_key, "workflow_blocked:wf-9");

        let cb = circuit_breaker_open("repo:o/r", "5 hook blocks");
        assert_eq!(cb.subject.entity.as_deref(), Some("repo:o/r"));

        let aging = ready_to_merge_aging("https://github.com/o/r/pull/1", 7200, Some("wf-9"));
        assert_eq!(
            aging.subject.pr_url.as_deref(),
            Some("https://github.com/o/r/pull/1")
        );
        assert_eq!(
            aging.dedup_key,
            "ready_to_merge_aging:https://github.com/o/r/pull/1"
        );
    }

    #[test]
    fn disabled_handle_spawns_no_watcher() {
        // No tokio runtime needed: the disabled path returns before spawning.
        assert!(spawn_notify_drop_watcher(AlertHandle::disabled()).is_none());
    }
}
