use crate::runtime_hosts::{RuntimeHostLifecycle, RuntimeHostManager};
use chrono::{TimeDelta, Utc};
use std::future::{poll_fn, Future};
use std::task::Poll;

#[test]
fn register_upserts_host_metadata() {
    let manager = RuntimeHostManager::with_heartbeat_timeout(60);

    let first = manager.register(
        "host-a".to_string(),
        Some("Host A".to_string()),
        vec!["rust".to_string()],
    );
    assert_eq!(first.display_name, "Host A");
    assert_eq!(first.capabilities, vec!["rust"]);

    let second = manager.register(
        "host-a".to_string(),
        Some("Host A updated".to_string()),
        vec!["rust".to_string(), "web".to_string()],
    );
    assert_eq!(second.display_name, "Host A updated");
    assert_eq!(second.capabilities, vec!["rust", "web"]);
    assert_eq!(manager.list_hosts().len(), 1);
}

#[test]
fn list_hosts_sorts_by_id() {
    let manager = RuntimeHostManager::with_heartbeat_timeout(60);
    manager.register("host-b".to_string(), None, vec![]);
    manager.register("host-a".to_string(), None, vec![]);

    let hosts = manager.list_hosts();
    let ids: Vec<&str> = hosts.iter().map(|host| host.id.as_str()).collect();
    assert_eq!(ids, vec!["host-a", "host-b"]);
}

#[test]
fn heartbeat_updates_existing_host() -> anyhow::Result<()> {
    let manager = RuntimeHostManager::with_heartbeat_timeout(60);
    manager.register("host-a".to_string(), None, vec![]);
    {
        let mut host = manager.hosts.get_mut("host-a").unwrap();
        host.last_heartbeat_at = Utc::now() - TimeDelta::seconds(30);
    }

    let before = manager.hosts.get("host-a").unwrap().last_heartbeat_at;
    let info = manager.heartbeat("host-a")?;
    let after = manager.hosts.get("host-a").unwrap().last_heartbeat_at;

    assert!(info.online);
    assert!(after >= before);
    Ok(())
}

#[test]
fn heartbeat_unknown_host_returns_error() {
    let manager = RuntimeHostManager::with_heartbeat_timeout(60);
    let err = manager.heartbeat("missing").unwrap_err();
    assert!(err.to_string().contains("is not registered"));
}

#[test]
fn list_hosts_marks_stale_heartbeat_offline() {
    let manager = RuntimeHostManager::with_heartbeat_timeout(10);
    manager.register("host-a".to_string(), None, vec![]);
    {
        let mut host = manager.hosts.get_mut("host-a").unwrap();
        host.last_heartbeat_at = Utc::now() - TimeDelta::seconds(30);
    }

    let hosts = manager.list_hosts();
    assert_eq!(hosts.len(), 1);
    assert!(!hosts[0].online);
}

#[test]
fn deregister_removes_host_only() {
    let manager = RuntimeHostManager::with_heartbeat_timeout(60);
    manager.register("host-a".to_string(), None, vec![]);

    assert!(manager.deregister("host-a"));
    assert!(!manager.deregister("host-a"));
    assert!(manager.list_hosts().is_empty());
}

#[test]
fn draining_host_cannot_be_reactivated_by_registration() {
    let manager = RuntimeHostManager::with_heartbeat_timeout(60);
    manager.register("host-a".to_string(), None, vec![]);

    assert_eq!(
        manager.mark_draining("host-a"),
        Some(RuntimeHostLifecycle::Active)
    );
    let updated = manager.register(
        "host-a".to_string(),
        Some("Updated host".to_string()),
        vec!["rust".to_string()],
    );

    assert_eq!(updated.lifecycle, RuntimeHostLifecycle::Draining);
    assert!(!manager.is_active("host-a"));
}

#[test]
fn registration_identity_changes_on_same_id_reregistration() {
    let manager = RuntimeHostManager::with_heartbeat_timeout(60);
    manager.register("host-a".to_string(), None, vec![]);
    let first = manager
        .active_registration_id("host-a")
        .expect("first registration must have an identity");

    assert!(manager.deregister("host-a"));
    manager.register("host-a".to_string(), None, vec![]);
    let second = manager
        .active_registration_id("host-a")
        .expect("replacement registration must have an identity");

    assert_ne!(first, second);
}

#[test]
fn persisted_legacy_host_defaults_to_active() {
    let value = serde_json::json!({
        "id": "host-a",
        "display_name": "Host A",
        "capabilities": [],
        "registered_at": "2026-07-16T00:00:00Z",
        "last_heartbeat_at": "2026-07-16T00:00:00Z"
    });
    let host: crate::runtime_hosts_state::PersistedRuntimeHost =
        serde_json::from_value(value).expect("legacy host record must deserialize");
    assert_eq!(host.lifecycle, RuntimeHostLifecycle::Active);
}

#[tokio::test]
async fn operation_lock_serializes_same_host_without_blocking_other_hosts() {
    let manager = RuntimeHostManager::with_heartbeat_timeout(60);
    let first = manager.lock_operation("host-a").await;
    let other = manager.lock_operation("host-b").await;
    assert_eq!(manager.operation_lock_count(), 2);
    drop(other);
    assert_eq!(manager.operation_lock_count(), 1);
    drop(first);
    assert_eq!(manager.operation_lock_count(), 0);
}

#[tokio::test]
async fn operation_lock_preserves_queued_order_across_deregister_and_reuse() {
    let manager = std::sync::Arc::new(RuntimeHostManager::with_heartbeat_timeout(60));
    manager.register("host-a".to_string(), None, vec![]);
    let first = manager.lock_operation("host-a").await;
    assert!(manager.deregister("host-a"));
    manager.register("host-a".to_string(), None, vec![]);

    let mut queued = Box::pin(manager.lock_operation("host-a"));
    poll_fn(|context| match queued.as_mut().poll(context) {
        Poll::Pending => Poll::Ready(()),
        Poll::Ready(_) => panic!("queued operation acquired the held lock"),
    })
    .await;
    drop(first);
    let second = queued.await;
    assert_eq!(manager.operation_lock_count(), 1);
    drop(second);
    assert_eq!(manager.operation_lock_count(), 0);
}

#[tokio::test]
async fn operation_lock_unknown_host_churn_does_not_accumulate_entries() {
    let manager = RuntimeHostManager::with_heartbeat_timeout(60);
    for index in 0..1_000 {
        let guard = manager.lock_operation(&format!("unknown-{index}")).await;
        drop(guard);
    }
    assert_eq!(manager.operation_lock_count(), 0);
}
