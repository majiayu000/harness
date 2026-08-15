use anyhow::Context;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static RUN_ID_SEQUENCE: AtomicU64 = AtomicU64::new(0);

pub(super) fn default_run_id(suite: &str) -> String {
    let now = chrono::Utc::now();
    let sequence = RUN_ID_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    format!(
        "{}-{}-{}-{sequence}",
        suite,
        now.format("%Y%m%dT%H%M%S%.9fZ"),
        std::process::id()
    )
}

pub(super) struct ReservedEvalOutput {
    path: PathBuf,
    file: File,
    committed: bool,
}

impl ReservedEvalOutput {
    pub(super) fn write_report<T: serde::Serialize>(&mut self, value: &T) -> anyhow::Result<()> {
        let payload = serde_json::to_vec_pretty(value)?;
        self.file
            .write_all(&payload)
            .with_context(|| format!("failed to write eval output {}", self.path.display()))?;
        self.file
            .sync_all()
            .with_context(|| format!("failed to sync eval output {}", self.path.display()))?;
        self.committed = true;
        Ok(())
    }
}

impl Drop for ReservedEvalOutput {
    fn drop(&mut self) {
        if !self.committed {
            let _ = fs::remove_file(&self.path);
        }
    }
}

pub(super) fn reserve_eval_output(path: &Path) -> anyhow::Result<ReservedEvalOutput> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent).with_context(|| {
            format!(
                "failed to create eval output directory {}",
                parent.display()
            )
        })?;
    }
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .with_context(|| {
            format!(
                "failed to reserve eval output {}; choose a new --run-id or --output",
                path.display()
            )
        })?;
    Ok(ReservedEvalOutput {
        path: path.to_path_buf(),
        file,
        committed: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_reservation_is_atomic_and_failed_reservations_are_released() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("report.json");
        let first = reserve_eval_output(&path).expect("first reservation");
        let error = reserve_eval_output(&path).err().expect("second must fail");
        assert!(error.to_string().contains("failed to reserve eval output"));

        drop(first);
        assert!(!path.exists());
        assert!(reserve_eval_output(&path).is_ok());
    }

    #[test]
    fn generated_run_ids_do_not_collide_within_a_process() {
        assert_ne!(default_run_id("suite"), default_run_id("suite"));
    }
}
