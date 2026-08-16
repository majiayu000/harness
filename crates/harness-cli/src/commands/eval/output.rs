use anyhow::Context;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

mod verification;
pub(super) use verification::{render_verification_evidence, render_verification_transition};

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

pub(super) fn eval_report_output_path(
    requested: Option<&PathBuf>,
    execute: bool,
    run_id: &str,
) -> anyhow::Result<Option<PathBuf>> {
    let output = requested
        .cloned()
        .or_else(|| execute.then(|| default_execute_output_path(run_id)));
    if execute {
        if let Some(path) = output.as_ref() {
            let exists = path.try_exists().with_context(|| {
                format!("failed to inspect eval output path {}", path.display())
            })?;
            if exists {
                anyhow::bail!(
                    "eval execute report already exists at {}; choose a new --run-id",
                    path.display()
                );
            }
        }
    }
    Ok(output)
}

pub(super) fn reserve_eval_run(
    project_root: &Path,
    run_id: &str,
    report_path: &Path,
) -> anyhow::Result<()> {
    reserve_eval_run_under(&project_root.join("artifacts/eval"), run_id, report_path)
}

fn reserve_eval_run_under(root: &Path, run_id: &str, report_path: &Path) -> anyhow::Result<()> {
    validate_run_id(run_id)?;
    let directory = root.join(run_id);
    fs::create_dir_all(&directory).with_context(|| {
        format!(
            "failed to create eval run reservation directory {}",
            directory.display()
        )
    })?;
    let marker = directory.join(".run-reservation.json");
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&marker)
        .with_context(|| {
            format!(
                "eval run id {run_id} is already reserved independently of --output; choose a new --run-id"
            )
        })?;
    let payload = serde_json::to_vec_pretty(&serde_json::json!({
        "run_id": run_id,
        "report_path": report_path,
    }))?;
    file.write_all(&payload)
        .with_context(|| format!("failed to write eval run reservation {}", marker.display()))?;
    file.sync_all()
        .with_context(|| format!("failed to sync eval run reservation {}", marker.display()))?;
    Ok(())
}

fn validate_run_id(run_id: &str) -> anyhow::Result<()> {
    let safe = !run_id.is_empty()
        && run_id.len() <= 200
        && run_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'));
    if !safe {
        anyhow::bail!(
            "eval --run-id must contain only ASCII letters, digits, '.', '-', or '_' and be at most 200 bytes"
        );
    }
    Ok(())
}

fn default_execute_output_path(run_id: &str) -> PathBuf {
    PathBuf::from("artifacts")
        .join("eval")
        .join(run_id)
        .join("report.json")
}

pub(super) fn rewrite_eval_report<T: serde::Serialize>(
    path: &Path,
    value: &T,
) -> anyhow::Result<()> {
    let temporary = path.with_extension(format!("event-retry-{}.tmp", std::process::id()));
    let result = (|| -> anyhow::Result<()> {
        {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)
                .with_context(|| {
                    format!("failed to create temporary report {}", temporary.display())
                })?;
            file.write_all(&serde_json::to_vec_pretty(value)?)?;
            file.sync_all()?;
        }
        fs::rename(&temporary, path).with_context(|| {
            format!(
                "failed to replace eval report {} with {}",
                path.display(),
                temporary.display()
            )
        })?;
        Ok(())
    })();
    if let Err(error) = result {
        if let Err(cleanup_error) = fs::remove_file(&temporary) {
            if cleanup_error.kind() != std::io::ErrorKind::NotFound {
                return Err(error.context(format!(
                    "also failed to remove temporary report {}: {cleanup_error}",
                    temporary.display()
                )));
            }
        }
        return Err(error);
    }
    Ok(())
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
    fn report_rewrite_replaces_an_existing_file() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("report.json");
        fs::write(&path, r#"{"state":"before"}"#).expect("initial report");

        rewrite_eval_report(&path, &serde_json::json!({ "state": "after" }))
            .expect("report rewrite");

        let report: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).expect("rewritten report should be readable"))
                .expect("rewritten report should be valid JSON");
        assert_eq!(report["state"], "after");
    }

    #[test]
    fn generated_run_ids_do_not_collide_within_a_process() {
        assert_ne!(default_run_id("suite"), default_run_id("suite"));
    }

    #[test]
    fn run_id_reservation_is_independent_of_report_path() {
        let directory = tempfile::tempdir().expect("tempdir");
        reserve_eval_run_under(directory.path(), "run-1", Path::new("first.json"))
            .expect("first reservation");

        let error = reserve_eval_run_under(directory.path(), "run-1", Path::new("second.json"))
            .expect_err("same run id must be rejected even with a different output");

        assert!(error.to_string().contains("already reserved"));
    }

    #[test]
    fn run_id_reservation_rejects_path_components() {
        let directory = tempfile::tempdir().expect("tempdir");
        assert!(
            reserve_eval_run_under(directory.path(), "../escape", Path::new("report.json"))
                .is_err()
        );
    }
}
