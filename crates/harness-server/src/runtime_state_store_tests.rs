use crate::runtime_project_cache::WatchedProjectInput;
use crate::runtime_state_store::{
    migrate_legacy_runtime_state_store_if_needed, RuntimeStateStore, RUNTIME_STATE_STORE_SCHEMA,
};
use crate::{http::build_app_state, server::HarnessServer, thread_manager::ThreadManager};
use chrono::Utc;
use futures::FutureExt;
use harness_agents::registry::AgentRegistry;
use harness_core::config::HarnessConfig;
use harness_core::db::{pg_open_pool, resolve_test_database_url, PgStoreContext, TestSchemaGuard};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

#[test]
fn shared_schema_context_uses_fixed_runtime_state_store_schema() -> anyhow::Result<()> {
    let context = RuntimeStateStore::shared_schema_context(Some(
        "postgres://user:pass@localhost:5432/harness",
    ))?;
    assert_eq!(context.schema(), RUNTIME_STATE_STORE_SCHEMA);
    assert!(
        context.ownership().is_none(),
        "shared runtime_state_store schema must not register path-derived ownership"
    );
    Ok(())
}

#[tokio::test]
async fn build_app_state_opens_runtime_state_store_from_shared_schema() -> anyhow::Result<()> {
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }
    let _lock = crate::test_helpers::HOME_LOCK.lock().await;
    let project_root = crate::test_helpers::tempdir_in_home("runtime-state-root-")?;
    let data_dir = tempfile::tempdir()?;

    let mut config = HarnessConfig::default();
    config.server.project_root = project_root.path().to_path_buf();
    config.server.data_dir = data_dir.path().to_path_buf();
    config.server.allow_unauthenticated = true;

    let server = Arc::new(HarnessServer::new(
        config,
        ThreadManager::new(),
        AgentRegistry::new("test"),
    ));
    let state = build_app_state(server).await?;
    let runtime_state_store = state
        .core
        .runtime_state_store
        .as_ref()
        .expect("runtime state store should be ready");
    assert_eq!(runtime_state_store.schema(), RUNTIME_STATE_STORE_SCHEMA);
    Ok(())
}

#[tokio::test]
async fn legacy_runtime_state_migration_backfills_once() -> anyhow::Result<()> {
    let database_url = match resolve_test_database_url(None) {
        Ok(url) => url,
        Err(_) => return Ok(()),
    };
    let dir = tempfile::tempdir()?;
    let target_data_dir = dir.path().join("target-data");
    let other_data_dir = dir.path().join("other-data");
    let legacy_path = target_data_dir.join("runtime_state.db");
    let legacy_schema = PgStoreContext::from_legacy_path_schema(&legacy_path, Some(&database_url))?
        .schema()
        .to_owned();
    let setup_pool = pg_open_pool(&database_url).await?;
    let mut target_schema = TestSchemaGuard::new(&database_url, "runtime_state_store_test")?;
    let target_context = PgStoreContext::from_schema(target_schema.schema(), Some(&database_url))?;
    let target_store = RuntimeStateStore::open_shared_with_data_dir(
        &target_context,
        &setup_pool,
        &target_data_dir,
    )
    .await?;
    let other_store =
        RuntimeStateStore::open_shared_with_data_dir(&target_context, &setup_pool, &other_data_dir)
            .await?;
    let legacy_store =
        RuntimeStateStore::open_with_database_url(&legacy_path, Some(&database_url)).await?;

    let result = std::panic::AssertUnwindSafe(async {
        legacy_store
            .persist_snapshot(vec![make_host("legacy-host")], vec![])
            .await?;

        let copied = migrate_legacy_runtime_state_store_if_needed(
            &legacy_path,
            Some(&database_url),
            &target_store,
        )
        .await?;
        assert_eq!(copied, 1, "one legacy snapshot should be copied");

        let copied_again = migrate_legacy_runtime_state_store_if_needed(
            &legacy_path,
            Some(&database_url),
            &target_store,
        )
        .await?;
        assert_eq!(copied_again, 0, "migration must be idempotent");

        let loaded = target_store
            .load_snapshot()
            .await?
            .expect("legacy snapshot should be present in the shared schema");
        assert_eq!(loaded.hosts.len(), 1);
        assert_eq!(loaded.hosts[0].id, "legacy-host");
        assert!(
            other_store.load_snapshot().await?.is_none(),
            "other data_dir scopes must not hydrate legacy runtime state"
        );

        target_store
            .persist_snapshot(vec![make_host("shared-host")], vec![])
            .await?;
        let copied_after_shared_update = migrate_legacy_runtime_state_store_if_needed(
            &legacy_path,
            Some(&database_url),
            &target_store,
        )
        .await?;
        assert_eq!(
            copied_after_shared_update, 0,
            "completed migration must not overwrite updated shared state"
        );
        let updated = target_store
            .load_snapshot()
            .await?
            .expect("updated shared snapshot should remain");
        assert_eq!(updated.hosts[0].id, "shared-host");

        delete_snapshot(&target_store).await?;
        let copied_after_shared_delete = migrate_legacy_runtime_state_store_if_needed(
            &legacy_path,
            Some(&database_url),
            &target_store,
        )
        .await?;
        assert_eq!(
            copied_after_shared_delete, 0,
            "completed migration must not resurrect deleted shared state"
        );
        assert!(
            target_store.load_snapshot().await?.is_none(),
            "deleted shared snapshot must stay deleted"
        );

        Ok::<(), anyhow::Error>(())
    })
    .catch_unwind()
    .await;

    legacy_store.pool().close().await;
    target_store.pool().close().await;
    other_store.pool().close().await;
    let _ = sqlx::query(&format!(
        "DROP SCHEMA IF EXISTS \"{legacy_schema}\" CASCADE"
    ))
    .execute(&setup_pool)
    .await;
    let cleanup_result = target_schema.cleanup_with_pool(&setup_pool).await;
    setup_pool.close().await;

    match result {
        Ok(result) => {
            cleanup_result?;
            result
        }
        Err(payload) => std::panic::resume_unwind(payload),
    }
}

#[tokio::test]
async fn build_app_state_restores_runtime_snapshot() -> anyhow::Result<()> {
    let _lock = crate::test_helpers::HOME_LOCK.lock().await;
    let project_root = crate::test_helpers::tempdir_in_home("runtime-state-root-")?;
    let data_dir = tempfile::tempdir()?;

    let mut config = HarnessConfig::default();
    config.server.project_root = project_root.path().to_path_buf();
    config.server.data_dir = data_dir.path().to_path_buf();
    config.server.allow_unauthenticated = true;

    let first_server = Arc::new(HarnessServer::new(
        config.clone(),
        ThreadManager::new(),
        AgentRegistry::new("test"),
    ));
    let first_state = build_app_state(first_server).await?;

    first_state.runtime_hosts.register(
        "host-a".to_string(),
        Some("Host A".to_string()),
        vec!["codex".to_string()],
    );
    first_state.runtime_project_cache.sync_host_projects(
        "host-a",
        vec![WatchedProjectInput {
            project_id: Some("default".to_string()),
            root: project_root
                .path()
                .canonicalize()?
                .to_string_lossy()
                .into_owned(),
        }],
    );
    first_state.persist_runtime_state().await?;
    drop(first_state);

    let second_server = Arc::new(HarnessServer::new(
        config,
        ThreadManager::new(),
        AgentRegistry::new("test"),
    ));
    let restored_state = build_app_state(second_server).await?;

    let hosts = restored_state.runtime_hosts.list_hosts();
    assert_eq!(hosts.len(), 1);
    assert_eq!(hosts[0].id, "host-a");

    let project_cache = restored_state
        .runtime_project_cache
        .get_host_cache("host-a")
        .expect("runtime project cache for host-a should be restored");
    assert_eq!(project_cache.project_count, 1);
    assert_eq!(
        project_cache.projects[0].project_id.as_deref(),
        Some("default")
    );
    Ok(())
}

async fn delete_snapshot(store: &RuntimeStateStore) -> anyhow::Result<()> {
    sqlx::query("DELETE FROM runtime_state WHERE store_key = $1")
        .bind(store.store_key())
        .execute(store.pool())
        .await?;
    Ok(())
}

fn make_host(id: &str) -> crate::runtime_hosts_state::PersistedRuntimeHost {
    crate::runtime_hosts_state::PersistedRuntimeHost {
        id: id.to_string(),
        display_name: id.to_string(),
        capabilities: vec!["codex".to_string()],
        registered_at: Utc::now(),
        last_heartbeat_at: Utc::now(),
        lifecycle: crate::runtime_hosts::RuntimeHostLifecycle::Active,
    }
}

#[test]
fn store_key_for_equivalent_relative_and_absolute_paths_matches() -> anyhow::Result<()> {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_nanos();
    let relative = PathBuf::from(format!("target/runtime_state_store_key_{stamp}"));
    std::fs::create_dir_all(&relative)?;
    let absolute = std::env::current_dir()?.join(&relative);

    let relative_key = RuntimeStateStore::store_key_for_data_dir(&relative)?;
    let absolute_key = RuntimeStateStore::store_key_for_data_dir(&absolute)?;
    assert_eq!(relative_key, absolute_key);
    assert_eq!(
        relative_key,
        absolute
            .canonicalize()?
            .to_str()
            .expect("utf8 canonical path")
    );

    std::fs::remove_dir_all(&relative)?;
    Ok(())
}

#[test]
fn store_key_creates_missing_directory_and_stays_stable() -> anyhow::Result<()> {
    let root = tempfile::tempdir()?;
    let missing = root.path().join("fresh-data-dir");
    assert!(!missing.exists());

    let first = RuntimeStateStore::store_key_for_data_dir(&missing)?;
    assert!(missing.is_dir());
    let second = RuntimeStateStore::store_key_for_data_dir(&missing)?;
    assert_eq!(first, second);
    assert_eq!(first, missing.canonicalize()?.to_str().expect("utf8"));
    Ok(())
}

#[cfg(unix)]
#[test]
fn store_key_follows_symlink_alias_to_target() -> anyhow::Result<()> {
    let root = tempfile::tempdir()?;
    let target = root.path().join("target-data");
    let alias = root.path().join("alias-data");
    std::fs::create_dir_all(&target)?;
    std::os::unix::fs::symlink(&target, &alias)?;

    let target_key = RuntimeStateStore::store_key_for_data_dir(&target)?;
    let alias_key = RuntimeStateStore::store_key_for_data_dir(&alias)?;
    assert_eq!(target_key, alias_key);
    Ok(())
}

#[test]
fn store_key_rejects_regular_file_and_child_beneath_file() -> anyhow::Result<()> {
    let root = tempfile::tempdir()?;
    let file_path = root.path().join("not-a-dir");
    std::fs::write(&file_path, b"content")?;

    let file_error = RuntimeStateStore::store_key_for_data_dir(&file_path)
        .expect_err("regular file must not become a store key");
    assert!(
        file_error
            .to_string()
            .contains("failed to create runtime state store data dir")
            || file_error
                .to_string()
                .contains("failed to canonicalize runtime state store data dir"),
        "unexpected error: {file_error:#}"
    );

    let nested = file_path.join("child");
    let nested_error = RuntimeStateStore::store_key_for_data_dir(&nested)
        .expect_err("child beneath a file must not become a store key");
    assert!(
        nested_error
            .to_string()
            .contains("failed to create runtime state store data dir"),
        "unexpected error: {nested_error:#}"
    );
    Ok(())
}

#[cfg(unix)]
#[test]
fn store_key_rejects_symlink_loop() -> anyhow::Result<()> {
    let root = tempfile::tempdir()?;
    let a = root.path().join("loop-a");
    let b = root.path().join("loop-b");
    std::os::unix::fs::symlink(&b, &a)?;
    std::os::unix::fs::symlink(&a, &b)?;

    let error = RuntimeStateStore::store_key_for_data_dir(&a)
        .expect_err("symlink loop must not produce a fallback store key");
    assert!(
        error
            .to_string()
            .contains("failed to create runtime state store data dir")
            || error
                .to_string()
                .contains("failed to canonicalize runtime state store data dir"),
        "unexpected error: {error:#}"
    );
    Ok(())
}

#[cfg(unix)]
#[test]
fn store_key_rejects_non_utf8_canonical_directory() -> anyhow::Result<()> {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;

    // Directly cover the lossless conversion boundary. Some filesystems (notably
    // macOS) reject non-UTF-8 path components at create_dir time (os error 92),
    // so a full create→canonicalize integration is not portable.
    let non_utf8 = PathBuf::from(OsStr::from_bytes(b"/tmp/runtime-state-\xff-dir"));
    let error = RuntimeStateStore::store_key_from_canonical_path(&non_utf8)
        .expect_err("non-UTF-8 canonical path must not use lossy identity");
    assert!(
        error.to_string().contains("not valid UTF-8"),
        "unexpected error: {error:#}"
    );

    let root = tempfile::tempdir()?;
    let non_utf8_name = OsStr::from_bytes(b"runtime-state-\xff-dir");
    let non_utf8_dir = root.path().join(non_utf8_name);
    match std::fs::create_dir_all(&non_utf8_dir) {
        Ok(()) => {
            let error = RuntimeStateStore::store_key_for_data_dir(&non_utf8_dir)
                .expect_err("non-UTF-8 canonical path must not use lossy identity");
            assert!(
                error.to_string().contains("not valid UTF-8"),
                "unexpected error: {error:#}"
            );
        }
        Err(error) => {
            assert!(
                error.raw_os_error() == Some(92)
                    || error.to_string().contains("Illegal byte sequence"),
                "unexpected create_dir failure (expected FS UTF-8 rejection): {error}"
            );
        }
    }
    Ok(())
}

#[tokio::test]
async fn open_shared_propagates_key_derivation_failure_without_fallback_namespace(
) -> anyhow::Result<()> {
    let database_url = match resolve_test_database_url(None) {
        Ok(url) => url,
        Err(_) => return Ok(()),
    };
    let root = tempfile::tempdir()?;
    let file_path = root.path().join("not-a-dir");
    std::fs::write(&file_path, b"content")?;

    let setup_pool = pg_open_pool(&database_url).await?;
    let mut schema = TestSchemaGuard::new(&database_url, "runtime_state_store_test")?;
    let context = PgStoreContext::from_schema(schema.schema(), Some(&database_url))?;

    let open_result =
        RuntimeStateStore::open_shared_with_data_dir(&context, &setup_pool, &file_path).await;
    let open_error = match open_result {
        Ok(_) => anyhow::bail!("key derivation failure must prevent namespace open"),
        Err(error) => error,
    };
    assert!(
        open_error
            .to_string()
            .contains("failed to create runtime state store data dir")
            || open_error
                .to_string()
                .contains("failed to canonicalize runtime state store data dir"),
        "unexpected error: {open_error:#}"
    );

    // A successful open on a real directory must not share identity with the
    // rejected raw file path that older code would have used as a fallback key.
    let real_dir = root.path().join("real-data");
    let store =
        RuntimeStateStore::open_shared_with_data_dir(&context, &setup_pool, &real_dir).await?;
    assert_ne!(store.store_key(), file_path.to_string_lossy().as_ref());
    store.pool().close().await;

    let cleanup_result = schema.cleanup_with_pool(&setup_pool).await;
    setup_pool.close().await;
    cleanup_result?;
    Ok(())
}

#[tokio::test]
async fn snapshot_roundtrip_preserves_namespace_across_equivalent_paths() -> anyhow::Result<()> {
    let database_url = match resolve_test_database_url(None) {
        Ok(url) => url,
        Err(_) => return Ok(()),
    };
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_nanos();
    let relative = PathBuf::from(format!("target/runtime_state_roundtrip_{stamp}"));
    std::fs::create_dir_all(&relative)?;
    let absolute = std::env::current_dir()?.join(&relative);

    let setup_pool = pg_open_pool(&database_url).await?;
    let mut schema = TestSchemaGuard::new(&database_url, "runtime_state_store_test")?;
    let context = PgStoreContext::from_schema(schema.schema(), Some(&database_url))?;

    let result = std::panic::AssertUnwindSafe(async {
        let first =
            RuntimeStateStore::open_shared_with_data_dir(&context, &setup_pool, &absolute).await?;
        first
            .persist_snapshot(vec![make_host("roundtrip-host")], vec![])
            .await?;
        let first_key = first.store_key().to_owned();

        let second =
            RuntimeStateStore::open_shared_with_data_dir(&context, &setup_pool, &relative).await?;
        assert_eq!(second.store_key(), first_key);
        let loaded = second
            .load_snapshot()
            .await?
            .expect("snapshot must remain under the canonical key");
        assert_eq!(loaded.hosts.len(), 1);
        assert_eq!(loaded.hosts[0].id, "roundtrip-host");

        first.pool().close().await;
        second.pool().close().await;
        Ok::<(), anyhow::Error>(())
    })
    .catch_unwind()
    .await;

    let cleanup_result = schema.cleanup_with_pool(&setup_pool).await;
    setup_pool.close().await;
    let _ = std::fs::remove_dir_all(&relative);
    match result {
        Ok(result) => {
            cleanup_result?;
            result
        }
        Err(payload) => {
            let _ = cleanup_result;
            std::panic::resume_unwind(payload)
        }
    }
}

#[tokio::test]
async fn overlapping_persist_calls_keep_newer_state_under_serialization_lock() -> anyhow::Result<()>
{
    if !crate::test_helpers::db_tests_enabled().await {
        return Ok(());
    }
    let _lock = crate::test_helpers::HOME_LOCK.lock().await;
    let project_root = crate::test_helpers::tempdir_in_home("runtime-state-overlap-root-")?;
    let data_dir = tempfile::tempdir()?;

    let mut config = HarnessConfig::default();
    config.server.project_root = project_root.path().to_path_buf();
    config.server.data_dir = data_dir.path().to_path_buf();
    config.server.allow_unauthenticated = true;

    let server = Arc::new(HarnessServer::new(
        config,
        ThreadManager::new(),
        AgentRegistry::new("test"),
    ));
    let state = Arc::new(build_app_state(server).await?);
    let store = state
        .core
        .runtime_state_store
        .as_ref()
        .expect("runtime state store should be ready")
        .clone();

    state.runtime_hosts.register(
        "host-old".to_string(),
        Some("Old".to_string()),
        vec!["codex".to_string()],
    );

    let lock_guard = state.runtime_state_persist_lock.lock().await;
    let state_for_task = state.clone();
    let ready = Arc::new(tokio::sync::Barrier::new(2));
    let task_ready = ready.clone();
    let persist_task = tokio::spawn(async move {
        task_ready.wait().await;
        state_for_task.persist_runtime_state().await
    });

    ready.wait().await;
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(
        !persist_task.is_finished(),
        "persist must wait on the serialization lock before capturing"
    );

    state.runtime_hosts.register(
        "host-new".to_string(),
        Some("New".to_string()),
        vec!["codex".to_string()],
    );
    drop(lock_guard);

    tokio::time::timeout(Duration::from_secs(5), persist_task).await???;

    let snapshot = store
        .load_snapshot()
        .await?
        .expect("overlapping persist must leave a durable snapshot");
    let host_ids: Vec<_> = snapshot.hosts.iter().map(|host| host.id.as_str()).collect();
    assert!(
        host_ids.contains(&"host-new"),
        "capture under the lock must observe the newer in-memory state; got {host_ids:?}"
    );
    assert!(
        host_ids.contains(&"host-old"),
        "earlier host registration must remain unless explicitly removed; got {host_ids:?}"
    );
    Ok(())
}
