use super::*;
use std::sync::{Mutex, OnceLock};

fn env_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

#[test]
fn run_id_resolution_prefers_request_env() {
    let mut env_vars = HashMap::new();
    env_vars.insert(
        AGENT_RUN_ID_ENV.to_string(),
        "ar-01j1qb3c9r7v5m2k8x4tznq6wd".to_string(),
    );
    env_vars.insert(
        AGENT_RUN_PARENT_ENV.to_string(),
        "ar-01j1qb3c9r7v5m2k8x4tznq6we".to_string(),
    );

    let identity = resolve_agent_run_identity(&env_vars);

    assert_eq!(identity.run_id.as_str(), "ar-01j1qb3c9r7v5m2k8x4tznq6wd");
    assert_eq!(
        identity.parent.as_ref().map(|id| id.as_str()),
        Some("ar-01j1qb3c9r7v5m2k8x4tznq6we")
    );
}

#[test]
fn run_id_resolution_mints_when_absent() {
    let _guard = env_lock().lock().unwrap();
    let original_id = std::env::var(AGENT_RUN_ID_ENV).ok();
    let original_parent = std::env::var(AGENT_RUN_PARENT_ENV).ok();
    unsafe { std::env::remove_var(AGENT_RUN_ID_ENV) };
    unsafe { std::env::remove_var(AGENT_RUN_PARENT_ENV) };

    let identity = resolve_agent_run_identity(&HashMap::new());

    assert!(identity.run_id.as_str().starts_with("ar-"));
    assert!(identity.parent.is_none());

    match original_id {
        Some(value) => unsafe { std::env::set_var(AGENT_RUN_ID_ENV, value) },
        None => unsafe { std::env::remove_var(AGENT_RUN_ID_ENV) },
    }
    match original_parent {
        Some(value) => unsafe { std::env::set_var(AGENT_RUN_PARENT_ENV, value) },
        None => unsafe { std::env::remove_var(AGENT_RUN_PARENT_ENV) },
    }
}

#[test]
fn run_id_provisional_binding_uses_harness_adapter_source() {
    let identity = RunIdentity::from_env_values(
        Some("ar-01j1qb3c9r7v5m2k8x4tznq6wd"),
        Some("ar-01j1qb3c9r7v5m2k8x4tznq6we"),
    )
    .expect("valid identity")
    .expect("identity");

    let record =
        provisional_agent_run_binding_record(&identity, "claude-code", 42, Path::new("/tmp/x"));

    assert_eq!(record.run_id.as_str(), "ar-01j1qb3c9r7v5m2k8x4tznq6wd");
    assert_eq!(
        record.parent.as_ref().map(|id| id.as_str()),
        Some("ar-01j1qb3c9r7v5m2k8x4tznq6we")
    );
    assert_eq!(record.native.kind, "claude-code");
    assert!(record.native.id.is_empty());
    assert_eq!(record.pid, 42);
    assert_eq!(record.source, "harness-adapter");
}
