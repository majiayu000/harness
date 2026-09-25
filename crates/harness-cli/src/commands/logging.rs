#[cfg(feature = "server")]
use chrono::{DateTime, NaiveDateTime, Utc};
#[cfg(feature = "server")]
use harness_server::server::RuntimeLogMetadata;
#[cfg(feature = "server")]
use std::cmp::Ordering;
use std::fs::File;
#[cfg(feature = "server")]
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
#[cfg(feature = "server")]
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tracing_subscriber::fmt::writer::MakeWriter;

use super::Command;

#[cfg(feature = "server")]
const RUNTIME_LOG_PREFIX: &str = "harness-serve-";
#[cfg(feature = "server")]
const RUNTIME_LOG_SUFFIX: &str = ".log";

#[derive(Clone)]
struct TeeMakeWriter {
    runtime_log_file: Option<Arc<Mutex<File>>>,
}

struct TeeWriter {
    stderr: io::Stderr,
    runtime_log_file: Option<Arc<Mutex<File>>>,
}

impl<'a> MakeWriter<'a> for TeeMakeWriter {
    type Writer = TeeWriter;

    fn make_writer(&'a self) -> Self::Writer {
        TeeWriter {
            stderr: io::stderr(),
            runtime_log_file: self.runtime_log_file.clone(),
        }
    }
}

impl Write for TeeWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.stderr.write_all(buf)?;
        if let Some(file) = &self.runtime_log_file {
            let mut guard = match file.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            guard.write_all(buf)?;
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.stderr.flush()?;
        if let Some(file) = &self.runtime_log_file {
            let mut guard = match file.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            guard.flush()?;
        }
        Ok(())
    }
}

pub(crate) struct LoggingBootstrap {
    #[cfg(feature = "server")]
    pub(crate) runtime_logs: RuntimeLogMetadata,
    pub(crate) runtime_log_file: Option<Arc<Mutex<File>>>,
    #[cfg(feature = "server")]
    pub(crate) setup_warning: Option<String>,
    #[cfg(feature = "server")]
    pub(crate) retention_warnings: Vec<String>,
}

pub(crate) fn init_tracing(bootstrap: &LoggingBootstrap) -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_timer(tracing_subscriber::fmt::time::ChronoLocal::new(
            "%Y-%m-%dT%H:%M:%S%.3f%:z".to_string(),
        ))
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "harness=info,warn".into()),
        )
        .with_writer(TeeMakeWriter {
            runtime_log_file: bootstrap.runtime_log_file.clone(),
        })
        .try_init()
        .map_err(|error| anyhow::anyhow!("failed to initialize tracing subscriber: {error}"))
}

pub(crate) fn prepare_logging(
    command: &Command,
    config: &harness_core::config::HarnessConfig,
) -> LoggingBootstrap {
    #[cfg(feature = "server")]
    if matches!(command, Command::Serve { .. }) {
        return prepare_runtime_logs(config, Utc::now());
    }
    #[cfg(not(feature = "server"))]
    let _ = (command, config);
    #[cfg(feature = "server")]
    let _ = command;
    LoggingBootstrap {
        #[cfg(feature = "server")]
        runtime_logs: RuntimeLogMetadata::disabled(
            config.observe.log_retention_days,
            config.observe.log_retention_max_files,
        ),
        runtime_log_file: None,
        #[cfg(feature = "server")]
        setup_warning: None,
        #[cfg(feature = "server")]
        retention_warnings: Vec::new(),
    }
}

#[cfg(feature = "server")]
fn prepare_runtime_logs(
    config: &harness_core::config::HarnessConfig,
    started_at: DateTime<Utc>,
) -> LoggingBootstrap {
    let retention_days = config.observe.log_retention_days;
    let retention_max_files = config.observe.log_retention_max_files;
    let log_path = runtime_log_path(&config.server.data_dir, started_at, std::process::id());
    let path_hint = RuntimeLogMetadata::public_path_hint(&log_path);

    match open_runtime_log_file(&log_path, retention_days, retention_max_files, started_at) {
        Ok((file, retention_warnings)) => LoggingBootstrap {
            runtime_logs: RuntimeLogMetadata::enabled(
                log_path,
                retention_days,
                retention_max_files,
            ),
            runtime_log_file: Some(Arc::new(Mutex::new(file))),
            setup_warning: None,
            retention_warnings,
        },
        Err(error) => LoggingBootstrap {
            runtime_logs: RuntimeLogMetadata::degraded(
                Some(path_hint),
                retention_days,
                retention_max_files,
            ),
            runtime_log_file: None,
            setup_warning: Some(error.to_string()),
            retention_warnings: Vec::new(),
        },
    }
}

#[cfg(feature = "server")]
pub(crate) fn runtime_log_path(data_dir: &Path, started_at: DateTime<Utc>, pid: u32) -> PathBuf {
    data_dir.join("logs").join(format!(
        "{RUNTIME_LOG_PREFIX}{}-pid{pid}{RUNTIME_LOG_SUFFIX}",
        started_at.format("%Y%m%dT%H%M%SZ")
    ))
}

#[cfg(feature = "server")]
fn open_runtime_log_file(
    log_path: &Path,
    retention_days: u32,
    retention_max_files: usize,
    started_at: DateTime<Utc>,
) -> io::Result<(File, Vec<String>)> {
    let logs_dir = log_path
        .parent()
        .ok_or_else(|| io::Error::other("runtime log path missing parent directory"))?;
    fs::create_dir_all(logs_dir)?;
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)?;
    let retention_warnings = purge_stale_runtime_logs(
        logs_dir,
        retention_days,
        retention_max_files,
        started_at,
        Some(log_path),
    );
    Ok((file, retention_warnings))
}

#[cfg(feature = "server")]
pub(crate) fn purge_stale_runtime_logs(
    logs_dir: &Path,
    retention_days: u32,
    retention_max_files: usize,
    now: DateTime<Utc>,
    protected_path: Option<&Path>,
) -> Vec<String> {
    purge_stale_runtime_logs_with(
        logs_dir,
        retention_days,
        retention_max_files,
        now,
        protected_path,
        |path| fs::remove_file(path),
    )
}

#[cfg(feature = "server")]
pub(crate) fn purge_stale_runtime_logs_with(
    logs_dir: &Path,
    retention_days: u32,
    retention_max_files: usize,
    now: DateTime<Utc>,
    protected_path: Option<&Path>,
    mut remove_file: impl FnMut(&Path) -> io::Result<()>,
) -> Vec<String> {
    if !logs_dir.exists() {
        return Vec::new();
    }

    let mut warnings = Vec::new();
    let mut retained = Vec::new();
    let cutoff = now - chrono::Duration::days(i64::from(retention_days));
    let entries = match fs::read_dir(logs_dir) {
        Ok(entries) => entries,
        Err(error) => {
            warnings.push(format!(
                "failed to scan runtime log directory {}: {error}",
                logs_dir.display()
            ));
            return warnings;
        }
    };
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                warnings.push(format!(
                    "failed to read runtime log directory entry in {}: {error}",
                    logs_dir.display()
                ));
                continue;
            }
        };
        let path = entry.path();
        if !path.is_file() {
            continue;
        }

        let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let Some((started_at, pid)) = parse_runtime_log_identity(file_name) else {
            continue;
        };
        let is_protected = protected_path == Some(path.as_path());
        if started_at < cutoff && !is_protected {
            if let Err(error) = remove_file(&path) {
                warnings.push(format!(
                    "failed to delete stale runtime log {}: {error}",
                    path.display()
                ));
            }
        } else {
            retained.push(RuntimeLogEntry {
                started_at,
                pid,
                path,
            });
        }
    }

    if retention_max_files > 0 && retained.len() > retention_max_files {
        retained.sort_by(|left, right| compare_runtime_logs(left, right, protected_path));
        for entry in retained.iter().skip(retention_max_files) {
            if let Err(error) = remove_file(&entry.path) {
                warnings.push(format!(
                    "failed to delete excess runtime log {}: {error}",
                    entry.path.display()
                ));
            }
        }
    }

    warnings
}

#[derive(Debug)]
#[cfg(feature = "server")]
struct RuntimeLogEntry {
    started_at: DateTime<Utc>,
    pid: u32,
    path: PathBuf,
}

#[cfg(feature = "server")]
fn compare_runtime_logs(
    left: &RuntimeLogEntry,
    right: &RuntimeLogEntry,
    protected_path: Option<&Path>,
) -> Ordering {
    let left_protected = protected_path == Some(left.path.as_path());
    let right_protected = protected_path == Some(right.path.as_path());
    right_protected
        .cmp(&left_protected)
        .then_with(|| right.started_at.cmp(&left.started_at))
        .then_with(|| right.pid.cmp(&left.pid))
        .then_with(|| left.path.cmp(&right.path))
}

#[cfg(feature = "server")]
fn parse_runtime_log_identity(file_name: &str) -> Option<(DateTime<Utc>, u32)> {
    let trimmed = file_name
        .strip_prefix(RUNTIME_LOG_PREFIX)?
        .strip_suffix(RUNTIME_LOG_SUFFIX)?;
    let (timestamp, pid) = trimmed.rsplit_once("-pid")?;
    let pid = pid.parse().ok()?;
    let naive = NaiveDateTime::parse_from_str(timestamp, "%Y%m%dT%H%M%SZ").ok()?;
    Some((DateTime::from_naive_utc_and_offset(naive, Utc), pid))
}
