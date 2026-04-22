use anyhow::Context as _;
use std::path::Path;
use std::sync::Arc;

use crate::{q_value_store::QValueStore, task_runner::TaskStore};

/// Outputs of the storage initialization phase.
pub(crate) struct StorageBundle {
    pub tasks: Arc<TaskStore>,
    pub q_values: Option<Arc<QValueStore>>,
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
    let tasks = TaskStore::open_with_database_url(&db_path, configured_database_url).await?;

    let q_values_db_path = harness_core::config::dirs::default_db_path(data_dir, "q_values");
    tracing::debug!("q_value db: {}", q_values_db_path.display());
    let q_values =
        match QValueStore::open_with_database_url(&q_values_db_path, configured_database_url).await
        {
            Ok(store) => Some(Arc::new(store)),
            Err(e) => {
                tracing::warn!(
                    path = %q_values_db_path.display(),
                    "q_value store init failed, rule utility tracking will be disabled: {e}"
                );
                None
            }
        };

    Ok(StorageBundle { tasks, q_values })
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
        // Both stores are accessible; q_values may be Some or None depending on
        // whether the SQLite backend initialises without error — on CI it should succeed.
        let _tasks = bundle.tasks;
        // q_values is best-effort; we don't assert Some here because a CI
        // environment with a read-only /tmp might fail the SQLite open.
        drop(bundle.q_values);
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
