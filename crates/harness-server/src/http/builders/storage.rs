use anyhow::Context as _;
use std::path::Path;
use std::sync::Arc;

use crate::http::state::StoreStartupResult;
use crate::{
    eval_store::{migrate_legacy_eval_store_if_needed, EvalStore},
    task_runner::TaskStore,
};

/// Outputs of the storage initialization phase.
pub(crate) struct StorageBundle {
    pub tasks: Option<Arc<TaskStore>>,
    pub eval_store: Option<Arc<EvalStore>>,
    pub startup_results: Vec<StoreStartupResult>,
}

fn failed_storage_startup_results(error: &str) -> Vec<StoreStartupResult> {
    vec![
        StoreStartupResult::critical("tasks").failed(error),
        StoreStartupResult::optional("eval_store").failed(error),
    ]
}

fn failed_storage_bundle(error: &str) -> StorageBundle {
    StorageBundle {
        tasks: None,
        eval_store: None,
        startup_results: failed_storage_startup_results(error),
    }
}

/// Initialize persistent storage: validate `data_dir`, open the task DB and
/// optional eval store.
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
    let eval_db_path = harness_core::config::dirs::default_db_path(data_dir, "evals");
    tracing::debug!("eval db: {}", eval_db_path.display());

    let database_url = match harness_core::db::resolve_database_url(configured_database_url) {
        Ok(database_url) => database_url,
        Err(error) => {
            let error = error.to_string();
            return Ok(failed_storage_bundle(&error));
        }
    };
    let setup_pool = match harness_core::db::pg_open_pool(&database_url).await {
        Ok(pool) => pool,
        Err(error) => {
            let error = error.to_string();
            return Ok(failed_storage_bundle(&error));
        }
    };

    let task_context = crate::task_db::TaskDb::shared_schema_context(Some(&database_url))?;
    super::ensure_startup_context_not_path_derived("tasks", &task_context)?;
    let (tasks, task_result) = match super::forced_startup_error("tasks") {
        Some(error) => (None, StoreStartupResult::critical("tasks").failed(error)),
        None => match TaskStore::open_shared_with_data_dir(
            &db_path,
            &task_context,
            &setup_pool,
            data_dir,
            Some(&database_url),
        )
        .await
        {
            Ok(store) => (Some(store), StoreStartupResult::critical("tasks")),
            Err(error) => (
                None,
                StoreStartupResult::critical("tasks").failed(error.to_string()),
            ),
        },
    };

    let (eval_store, eval_result) = match super::forced_startup_error("eval_store") {
        Some(error) => (
            None,
            StoreStartupResult::optional("eval_store").failed(error),
        ),
        None => {
            match async {
                let eval_context = EvalStore::shared_schema_context(Some(&database_url))?;
                super::ensure_startup_context_not_path_derived("eval_store", &eval_context)?;
                let store =
                    EvalStore::open_shared_with_data_dir(&eval_context, &setup_pool, data_dir)
                        .await?;
                migrate_legacy_eval_store_if_needed(&eval_db_path, Some(&database_url), &store)
                    .await?;
                Ok::<_, anyhow::Error>(store)
            }
            .await
            {
                Ok(store) => (
                    Some(Arc::new(store)),
                    StoreStartupResult::optional("eval_store"),
                ),
                Err(error) => (
                    None,
                    StoreStartupResult::optional("eval_store").failed(error.to_string()),
                ),
            }
        }
    };

    if let Some(error) = eval_result.error.as_deref() {
        tracing::warn!(
            path = %eval_db_path.display(),
            "eval store init failed, eval persistence APIs will be disabled: {error}"
        );
    }

    setup_pool.close().await;

    Ok(StorageBundle {
        tasks,
        eval_store,
        startup_results: vec![task_result, eval_result],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn happy_path_storage_stores_open() {
        let database_url = match harness_core::db::resolve_test_database_url(None) {
            Ok(url) => url,
            Err(_) => return,
        };
        let dir = tempfile::tempdir().expect("tempdir");
        let bundle = build_storage_with_database_url(dir.path(), Some(&database_url))
            .await
            .expect("build_storage should succeed");
        assert!(
            bundle.tasks.is_some(),
            "tasks store should be ready: {:?}",
            bundle.startup_results
        );
        assert!(
            bundle.eval_store.is_some(),
            "eval store should be ready: {:?}",
            bundle.startup_results
        );
    }

    #[tokio::test]
    async fn build_storage_opens_task_store_from_shared_schema() {
        let database_url = match harness_core::db::resolve_test_database_url(None) {
            Ok(url) => url,
            Err(_) => return,
        };
        let dir = tempfile::tempdir().expect("tempdir");
        let bundle = build_storage_with_database_url(dir.path(), Some(&database_url))
            .await
            .expect("build_storage should succeed");
        let tasks = bundle.tasks.expect("tasks store should be ready");

        assert_eq!(
            tasks.task_db_schema_for_test(),
            crate::task_db::TASK_DB_SCHEMA
        );
        assert_eq!(
            tasks.task_db_store_key_for_test(),
            crate::task_db::TaskDb::store_key_for_data_dir(dir.path()).expect("store key")
        );
    }

    #[tokio::test]
    async fn build_storage_opens_eval_store_from_shared_schema() {
        let database_url = match harness_core::db::resolve_test_database_url(None) {
            Ok(url) => url,
            Err(_) => return,
        };
        let dir = tempfile::tempdir().expect("tempdir");
        let bundle = build_storage_with_database_url(dir.path(), Some(&database_url))
            .await
            .expect("build_storage should succeed");
        let eval_store = bundle.eval_store.expect("eval store should be ready");

        assert_eq!(eval_store.schema(), crate::eval_store::EVAL_STORE_SCHEMA);
        assert_eq!(
            eval_store.store_key(),
            crate::eval_store::EvalStore::store_key_for_data_dir(dir.path()).expect("store key")
        );
    }

    #[test]
    fn bootstrap_failure_records_all_storage_statuses() {
        let statuses = failed_storage_startup_results("database unavailable");
        assert_eq!(statuses.len(), 2);
        assert_eq!(statuses[0].name, "tasks");
        assert!(statuses[0].is_critical());
        assert!(!statuses[0].ready);
        assert_eq!(statuses[1].name, "eval_store");
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
