use anyhow::Context as _;
use std::path::Path;
use std::sync::Arc;

use crate::http::state::StoreStartupResult;
use crate::{q_value_store::QValueStore, task_runner::TaskStore};

/// Outputs of the storage initialization phase.
pub(crate) struct StorageBundle {
    pub tasks: Option<Arc<TaskStore>>,
    pub q_values: Option<Arc<QValueStore>>,
    pub startup_results: Vec<StoreStartupResult>,
}

fn failed_storage_startup_results(error: &str) -> Vec<StoreStartupResult> {
    vec![
        StoreStartupResult::critical("tasks").failed(error),
        StoreStartupResult::optional("q_value_store").failed(error),
    ]
}

/// Initialize persistent storage: validate `data_dir`, open the task DB and
/// optionally the Q-value DB.
///
/// On Unix this function refuses to proceed if `data_dir` is a symbolic link
/// to prevent symlink-hijacking attacks on the persistent data directory.
#[cfg(test)]
pub(crate) async fn build_storage(data_dir: &Path) -> anyhow::Result<StorageBundle> {
    build_storage_with_database_url(data_dir, None).await
}

pub(crate) async fn build_storage_with_database_url(
    data_dir: &Path,
    configured_database_url: Option<&str>,
) -> anyhow::Result<StorageBundle> {
    std::fs::create_dir_all(data_dir)?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let meta = std::fs::symlink_metadata(data_dir)
            .with_context(|| format!("failed to stat data_dir {:?}", data_dir))?;
        if meta.file_type().is_symlink() {
            anyhow::bail!(
                "data_dir {:?} is a symbolic link; refusing to start to prevent \
                 potential symlink hijacking. Set an explicit `data_dir` in your \
                 harness config file.",
                data_dir
            );
        }
        std::fs::set_permissions(data_dir, std::fs::Permissions::from_mode(0o700)).with_context(
            || format!("failed to set 0o700 permissions on data_dir {:?}", data_dir),
        )?;
    }

    let db_path = harness_core::config::dirs::default_db_path(data_dir, "tasks");
    tracing::debug!("task db: {}", db_path.display());
    let q_values_db_path = harness_core::config::dirs::default_db_path(data_dir, "q_values");
    tracing::debug!("q_value db: {}", q_values_db_path.display());

    let database_url = match harness_core::db::resolve_database_url(configured_database_url) {
        Ok(database_url) => database_url,
        Err(error) => {
            let error = error.to_string();
            return Ok(StorageBundle {
                tasks: None,
                q_values: None,
                startup_results: failed_storage_startup_results(&error),
            });
        }
    };
    let setup_pool = match harness_core::db::pg_open_pool(&database_url).await {
        Ok(pool) => pool,
        Err(error) => {
            let error = error.to_string();
            return Ok(StorageBundle {
                tasks: None,
                q_values: None,
                startup_results: failed_storage_startup_results(&error),
            });
        }
    };

    let task_context = harness_core::db::PgStoreContext::new(
        database_url.clone(),
        harness_core::db::pg_schema_for_path(&db_path)?,
    )?;
    let (tasks, task_result) = match super::forced_startup_error("tasks") {
        Some(error) => (None, StoreStartupResult::critical("tasks").failed(error)),
        None => match TaskStore::open_with_context(&db_path, &task_context, &setup_pool).await {
            Ok(store) => (Some(store), StoreStartupResult::critical("tasks")),
            Err(error) => (
                None,
                StoreStartupResult::critical("tasks").failed(error.to_string()),
            ),
        },
    };

    let (q_values, q_value_result) = match super::forced_startup_error("q_value_store") {
        Some(error) => (
            None,
            StoreStartupResult::optional("q_value_store").failed(error),
        ),
        None => match harness_core::db::pg_schema_for_path(&q_values_db_path)
            .and_then(|schema| harness_core::db::PgStoreContext::new(database_url, schema))
        {
            Ok(q_value_context) => {
                match QValueStore::open_with_context(&q_value_context, &setup_pool).await {
                    Ok(store) => (
                        Some(Arc::new(store)),
                        StoreStartupResult::optional("q_value_store"),
                    ),
                    Err(error) => (
                        None,
                        StoreStartupResult::optional("q_value_store").failed(error.to_string()),
                    ),
                }
            }
            Err(error) => (
                None,
                StoreStartupResult::optional("q_value_store").failed(error.to_string()),
            ),
        },
    };

    if let Some(error) = q_value_result.error.as_deref() {
        tracing::warn!(
            path = %q_values_db_path.display(),
            "q_value store init failed, rule utility tracking will be disabled: {error}"
        );
    }

    setup_pool.close().await;

    Ok(StorageBundle {
        tasks,
        q_values,
        startup_results: vec![task_result, q_value_result],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn happy_path_both_dbs_open() {
        let dir = tempfile::tempdir().expect("tempdir");
        let bundle = build_storage(dir.path())
            .await
            .expect("build_storage should succeed");
        assert!(bundle.tasks.is_some(), "tasks store should be ready");
        drop(bundle.q_values);
    }

    #[tokio::test]
    async fn q_value_failure_is_recorded_as_optional() {
        let dir = tempfile::tempdir().expect("tempdir");
        let bundle = super::super::with_forced_startup_failures(
            &[(
                "q_value_store",
                "pool timed out while waiting for an open connection",
            )],
            build_storage(dir.path()),
        )
        .await
        .expect("build_storage should succeed");
        assert!(
            bundle.tasks.is_some(),
            "critical task store should still open"
        );
        assert!(
            bundle.q_values.is_none(),
            "optional q_value store should stay disabled"
        );
        assert_eq!(bundle.startup_results.len(), 2);
        assert!(bundle.startup_results[0].ready);
        assert!(!bundle.startup_results[1].ready);
        assert!(!bundle.startup_results[1].is_critical());
    }

    #[test]
    fn bootstrap_failure_records_all_storage_statuses() {
        let statuses = failed_storage_startup_results("database unavailable");
        assert_eq!(statuses.len(), 2);
        assert_eq!(statuses[0].name, "tasks");
        assert!(statuses[0].is_critical());
        assert!(!statuses[0].ready);
        assert_eq!(statuses[1].name, "q_value_store");
        assert!(!statuses[1].is_critical());
        assert!(!statuses[1].ready);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn symlink_data_dir_returns_err() {
        let real_dir = tempfile::tempdir().expect("real tempdir");
        let link_path = real_dir.path().join("symlink_data");
        std::os::unix::fs::symlink(real_dir.path(), &link_path).expect("create symlink");
        let result = build_storage(&link_path).await;
        let err = result.err().expect("expected Err for symlink data_dir");
        let msg = err.to_string();
        assert!(
            msg.contains("symbolic link"),
            "error should mention symbolic link, got: {msg}"
        );
    }
}
