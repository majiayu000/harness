use crate::runtime_hosts::RuntimeHostManager;
use chrono::{TimeDelta, Utc};

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
