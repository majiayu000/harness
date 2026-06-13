use crate::runtime_project_cache::WatchedProjectInput;
use crate::runtime_state_store::{
    migrate_legacy_runtime_state_store_if_needed, RuntimeStateStore, RUNTIME_STATE_STORE_SCHEMA,
};
use crate::{http::build_app_state, server::HarnessServer, thread_manager::ThreadManager};
use chrono::Utc;
use futures::FutureExt;
use harness_agents::registry::AgentRegistry;
use harness_core::config::HarnessConfig;
use harness_core::db::{pg_open_pool, resolve_database_url, PgStoreContext};
use std::sync::Arc;

fn unique_test_schema(prefix: &str) -> String {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let count = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock before UNIX epoch")
        .as_nanos();
    format!("{prefix}_{nanos}_{count}")
}

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
    let _lock = crate::test_helpers::HOME_LOCK.lock().await;
    let project_root = crate::test_helpers::tempdir_in_home("runtime-state-root-")?;
    let data_dir = tempfile::tempdir()?;

    let mut config = HarnessConfig::default();
    config.server.project_root = project_root.path().to_path_buf();
    config.server.data_dir = data_dir.path().to_path_buf();

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
    let database_url = match resolve_database_url(None) {
        Ok(url) => url,
        Err(_) => return Ok(()),
    };
    let dir = tempfile::tempdir()?;
    let target_data_dir = dir.path().join("target-data");
    let other_data_dir = dir.path().join("other-data");
    let legacy_path = target_data_dir.join("runtime_state.db");
    let legacy_schema = PgStoreContext::from_path(&legacy_path, Some(&database_url))?
        .schema()
        .to_owned();
    let target_schema = unique_test_schema("runtime_state_store_test");
    let setup_pool = pg_open_pool(&database_url).await?;
    let target_context = PgStoreContext::from_schema(&target_schema, Some(&database_url))?;
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
    let _ = sqlx::query(&format!(
        "DROP SCHEMA IF EXISTS \"{target_schema}\" CASCADE"
    ))
    .execute(&setup_pool)
    .await;
    setup_pool.close().await;

    match result {
        Ok(result) => result,
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
    }
}
