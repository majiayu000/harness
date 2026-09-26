use crate::handlers::validate_file_in_root;
use crate::http::api_error::ApiError;
use crate::http::rest_contract::{ContractJson, ContractQuery};
use crate::http::AppState;
use axum::extract::State;
use chrono::{DateTime, Utc};
use harness_protocol::rest::{
    EvalRunEntry, EvalRunListQuery, EvalRunListResponse, EvalRunRequest, EvalRunResponse,
};
use harness_workflow::runtime::{eval_report_dry_run, parse_benchmark_manifest_str, EvalRunReport};
#[cfg(unix)]
use harness_workflow::runtime::{execute_manifest, EvalEventPersistenceError, EvalExecuteConfig};
#[cfg(unix)]
use sqlx::Connection;
#[cfg(any(test, not(unix)))]
use std::fs;
#[cfg(unix)]
use std::fs::File;
#[cfg(unix)]
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
#[cfg(unix)]
use std::sync::OnceLock;

#[cfg(unix)]
#[derive(serde::Serialize, serde::Deserialize)]
struct EvalRunMarker {
    run_id: String,
    suite: String,
    started_at: String,
    pid: u32,
    status: String,
    error: Option<String>,
}

#[cfg(unix)]
const MAX_EVAL_RUNS: usize = 20;

#[cfg(unix)]
static ACTIVE_EVAL_PROJECTS: OnceLock<dashmap::DashMap<PathBuf, String>> = OnceLock::new();

#[cfg(unix)]
struct ActiveEvalProject {
    root: PathBuf,
    run_id: String,
}

#[cfg(unix)]
impl ActiveEvalProject {
    fn acquire(root: &Path, run_id: &str) -> Result<Self, ApiError> {
        let active = ACTIVE_EVAL_PROJECTS.get_or_init(dashmap::DashMap::new);
        match active.entry(root.to_path_buf()) {
            dashmap::mapref::entry::Entry::Vacant(entry) => {
                entry.insert(run_id.to_string());
            }
            dashmap::mapref::entry::Entry::Occupied(_) => {
                return Err(ApiError::BadRequest(
                    "an eval is already running for this project".to_string(),
                ));
            }
        }
        Ok(Self {
            root: root.to_path_buf(),
            run_id: run_id.to_string(),
        })
    }
}

#[cfg(unix)]
impl Drop for ActiveEvalProject {
    fn drop(&mut self) {
        if let Some(active) = ACTIVE_EVAL_PROJECTS.get() {
            if let dashmap::mapref::entry::Entry::Occupied(entry) = active.entry(self.root.clone())
            {
                if entry.get() == &self.run_id {
                    entry.remove();
                }
            }
        }
    }
}

#[cfg(unix)]
fn eval_lock_key(root: &Path) -> String {
    format!("harness-console-eval:{}", root.display())
}

#[cfg(unix)]
async fn eval_database_lock_held(pool: &sqlx::PgPool, root: &Path) -> Result<bool, ApiError> {
    if local_active_run_id(root).is_some() {
        return Ok(true);
    }
    let mut connection = pool.acquire().await.map_err(internal)?;
    connection.close_on_drop();
    let acquired: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock(hashtextextended($1, 0))")
        .bind(eval_lock_key(root))
        .fetch_one(&mut *connection)
        .await
        .map_err(internal)?;
    Ok(!acquired)
}

#[cfg(unix)]
async fn acquire_eval_database_lock(
    pool: &sqlx::PgPool,
    root: &Path,
) -> Result<sqlx::PgConnection, ApiError> {
    let options = pool.connect_options();
    let mut connection = sqlx::PgConnection::connect_with(options.as_ref())
        .await
        .map_err(internal)?;
    let acquired: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock(hashtextextended($1, 0))")
        .bind(eval_lock_key(root))
        .fetch_one(&mut connection)
        .await
        .map_err(internal)?;
    if !acquired {
        if let Err(error) = connection.close().await {
            tracing::error!(%error, "failed to close rejected eval lock connection");
        }
        return Err(ApiError::BadRequest(
            "an eval is already running for this project".to_string(),
        ));
    }
    Ok(connection)
}

pub(crate) async fn list_eval_runs(
    State(state): State<Arc<AppState>>,
    ContractQuery(query): ContractQuery<EvalRunListQuery>,
) -> Result<ContractJson<EvalRunListResponse>, ApiError> {
    require_secure_eval_platform()?;
    let root = project_root(&state, &query.project_root).await?;
    #[cfg(not(unix))]
    {
        let _ = root;
        return Err(ApiError::Internal(
            "secure eval report listing is unavailable on this platform".to_string(),
        ));
    }
    #[cfg(unix)]
    {
        let mut errors = Vec::new();
        let (active, lock_confirmed) = match state.workflow_runtime_store() {
            Ok(store) => match eval_database_lock_held(store.pool(), &root).await {
                Ok(active) => (active, true),
                Err(error) => {
                    errors.push(format!("eval activity unavailable: {error}"));
                    (true, false)
                }
            },
            Err(error) => {
                errors.push(format!("eval activity unavailable: {error}"));
                (true, false)
            }
        };
        let (candidates, scan_errors, unresolved) = scan_eval_candidates(&root)?;
        errors.extend(scan_errors);
        let local_active_run = local_active_run_id(&root);
        let mut active_claimed = false;
        let mut runs = Vec::new();
        for candidate in candidates {
            if runs.len() == MAX_EVAL_RUNS {
                break;
            }
            let result = match candidate.report.as_ref() {
                Some(report) => {
                    read_report_entry(report, candidate.modified, candidate.marker.as_ref())
                }
                None => match candidate.marker {
                    Some(marker) => read_marker_entry(
                        marker,
                        active,
                        lock_confirmed,
                        local_active_run.as_deref(),
                        &mut active_claimed,
                    ),
                    None => continue,
                },
            };
            match result {
                Ok(run) => runs.push(run),
                Err(error) => errors.push(format!("{}: {error}", candidate.label)),
            }
        }
        for error in &errors {
            tracing::error!(%error, "console eval report unavailable");
        }
        Ok(ContractJson(EvalRunListResponse {
            runs,
            errors,
            active,
            unresolved,
        }))
    }
}

pub(crate) async fn run_eval(
    State(state): State<Arc<AppState>>,
    ContractJson(request): ContractJson<EvalRunRequest>,
) -> Result<ContractJson<EvalRunResponse>, ApiError> {
    require_secure_eval_platform()?;
    let root = project_root(&state, &request.project_root).await?;
    let requested_path = Path::new(&request.manifest_path);
    let manifest_path = if requested_path.is_absolute() {
        requested_path.to_path_buf()
    } else {
        root.join(requested_path)
    };
    let manifest_path =
        validate_file_in_root(&manifest_path, &root).map_err(ApiError::BadRequest)?;
    if !manifest_path.is_file() {
        return Err(ApiError::BadRequest(
            "eval manifest is not a file".to_string(),
        ));
    }
    #[cfg(unix)]
    let content = read_project_file(&root, &manifest_path)?;
    #[cfg(not(unix))]
    let content = fs::read_to_string(&manifest_path).map_err(internal)?;
    let manifest = parse_benchmark_manifest_str(&content)
        .map_err(|error| ApiError::BadRequest(format!("invalid eval manifest: {error}")))?;
    let run_id = format!("console-{}", uuid::Uuid::new_v4());
    if request.dry_run {
        let report = eval_report_dry_run(&manifest, run_id, 3)
            .map_err(|error| ApiError::BadRequest(error.to_string()))?;
        return Ok(ContractJson(EvalRunResponse {
            run: entry(&report, Utc::now(), "dry_run")?,
        }));
    }

    #[cfg(unix)]
    {
        reject_during_maintenance(&state.core.server.config.maintenance_window, Utc::now())?;
        if !crate::services::execution::workflow_runtime_loops_enabled(&root)? {
            return Err(ApiError::Internal(
                "workflow runtime dispatch and worker must be enabled for evals".to_string(),
            ));
        }
        let store = state.workflow_runtime_store()?.clone();
        let events = state.observability.events.clone();
        let active = ActiveEvalProject::acquire(&root, &run_id)?;
        let database_lock = acquire_eval_database_lock(store.pool(), &root).await?;
        let (_, _, unresolved) = scan_eval_candidates(&root)?;
        if unresolved {
            if let Err(error) = database_lock.close().await {
                tracing::error!(%error, "failed to close rejected eval database lock");
            }
            return Err(ApiError::BadRequest(
                "a previous Console eval is unresolved; inspect its workflows and remove run-state.json only after cleanup".to_string(),
            ));
        }
        let output = EvalReportOutput::create(&root, &run_id)?;
        let marker = EvalRunMarker {
            run_id: run_id.clone(),
            suite: manifest.suite.clone(),
            started_at: Utc::now().to_rfc3339(),
            pid: std::process::id(),
            status: "running".to_string(),
            error: None,
        };
        output.write_marker(&marker)?;
        let project_id = root.to_string_lossy().into_owned();
        let task = tokio::spawn(async move {
            let _active = active;
            let result: Result<ContractJson<EvalRunResponse>, ApiError> = async {
                let config = EvalExecuteConfig::new(run_id.clone(), project_id.clone(), 3);
                let report = match execute_manifest(&store, &events, &manifest, config).await {
                    Ok(report) => report,
                    Err(error) => {
                        if let Some(partial) = error.downcast_ref::<EvalEventPersistenceError>() {
                            output.write(partial.report())?;
                        }
                        return Err(ApiError::Internal(error.to_string()));
                    }
                };
                output.write(&report)?;
                entry(&report, Utc::now(), "completed")
                    .map(|run| ContractJson(EvalRunResponse { run }))
            }
            .await;
            if let Err(error) = &result {
                let failed = EvalRunMarker {
                    status: "failed".to_string(),
                    error: Some(error.to_string()),
                    ..marker
                };
                if let Err(marker_error) = output.write_marker(&failed) {
                    tracing::error!(run_id = %run_id, project = %project_id, %marker_error, "failed to record console eval failure");
                }
                tracing::error!(run_id = %run_id, project = %project_id, %error, "console eval run failed");
            }
            if let Err(error) = database_lock.close().await {
                tracing::error!(run_id = %run_id, project = %project_id, %error, "failed to close console eval database lock");
            }
            result
        });
        task.await
            .map_err(|error| ApiError::Internal(format!("eval task failed: {error}")))?
    }
    #[cfg(not(unix))]
    {
        Err(ApiError::Internal(
            "secure eval report storage is unavailable on this platform".to_string(),
        ))
    }
}

async fn project_root(state: &AppState, requested: &str) -> Result<PathBuf, ApiError> {
    if requested.trim().is_empty() {
        return Err(ApiError::BadRequest("project root is empty".to_string()));
    }
    let root = Path::new(requested)
        .canonicalize()
        .map_err(|error| ApiError::BadRequest(format!("invalid project root: {error}")))?;
    if !root.is_dir() {
        return Err(ApiError::BadRequest(
            "project root is not a directory".to_string(),
        ));
    }
    if state
        .project_svc
        .default_root()
        .canonicalize()
        .ok()
        .as_ref()
        == Some(&root)
    {
        return Ok(root);
    }
    let projects = state.project_svc.list().await.map_err(internal)?;
    if projects
        .iter()
        .any(|project| project.active && project.root.canonicalize().ok().as_ref() == Some(&root))
    {
        return Ok(root);
    }
    Err(ApiError::BadRequest(
        "project root is not registered".to_string(),
    ))
}

fn require_secure_eval_platform() -> Result<(), ApiError> {
    #[cfg(unix)]
    {
        Ok(())
    }
    #[cfg(not(unix))]
    {
        Err(ApiError::Internal(
            "secure eval file access is unavailable on this platform".to_string(),
        ))
    }
}

#[cfg(unix)]
fn reject_during_maintenance(
    maintenance: &harness_core::config::maintenance::MaintenanceWindowConfig,
    now: DateTime<Utc>,
) -> Result<(), ApiError> {
    if maintenance.in_quiet_window(now) {
        return Err(ApiError::MaintenanceWindow {
            retry_after_secs: maintenance.secs_until_window_end(now),
        });
    }
    Ok(())
}

#[cfg(unix)]
fn read_report_entry(
    report: &EvalRunReport,
    modified: std::time::SystemTime,
    marker: Option<&EvalRunMarker>,
) -> Result<EvalRunEntry, ApiError> {
    let mut run = entry(report, DateTime::<Utc>::from(modified), "completed")?;
    if let Some(marker) = marker {
        if marker.status == "failed"
            && marker.run_id == report.run_id
            && report
                .outcome
                .is_some_and(|outcome| outcome.has_event_persistence_failure())
        {
            run.status = "failed".to_string();
            run.error = marker.error.clone();
        }
    }
    Ok(run)
}

#[cfg(unix)]
fn read_marker_entry(
    marker: EvalRunMarker,
    active: bool,
    lock_confirmed: bool,
    local_active_run: Option<&str>,
    active_claimed: &mut bool,
) -> Result<EvalRunEntry, ApiError> {
    let status = match marker.status.as_str() {
        "failed" => "failed",
        "running" if !lock_confirmed => "unknown",
        "running"
            if active
                && !*active_claimed
                && local_active_run.is_none_or(|run_id| run_id == marker.run_id) =>
        {
            *active_claimed = true;
            "running"
        }
        "running" => "interrupted",
        _ => return Err(ApiError::Internal("invalid eval run state".to_string())),
    };
    Ok(EvalRunEntry {
        run_id: marker.run_id,
        suite: marker.suite,
        status: status.to_string(),
        report: serde_json::Value::Null,
        reported_at: marker.started_at,
        error: marker.error,
    })
}

#[cfg(unix)]
fn local_active_run_id(root: &Path) -> Option<String> {
    ACTIVE_EVAL_PROJECTS
        .get()
        .and_then(|active| active.get(root).map(|entry| entry.value().clone()))
}

#[cfg(unix)]
fn create_or_open_child(parent: &File, name: &str) -> Result<File, ApiError> {
    match rustix::fs::mkdirat(parent, name, rustix::fs::Mode::RWXU) {
        Ok(()) | Err(rustix::io::Errno::EXIST) => {}
        Err(error) => return Err(internal(error)),
    }
    open_child(parent, name)
}

#[cfg(unix)]
fn open_child(parent: &File, name: &str) -> Result<File, ApiError> {
    rustix::fs::openat(
        parent,
        name,
        rustix::fs::OFlags::RDONLY
            | rustix::fs::OFlags::DIRECTORY
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    )
    .map(File::from)
    .map_err(|error| ApiError::BadRequest(format!("unsafe eval report directory {name}: {error}")))
}

#[cfg(unix)]
fn read_project_file(root: &Path, path: &Path) -> Result<String, ApiError> {
    let relative = path
        .strip_prefix(root)
        .map_err(|_| ApiError::BadRequest("eval manifest is outside project root".to_string()))?;
    let mut directory = File::from(
        rustix::fs::open(
            root,
            rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::DIRECTORY
                | rustix::fs::OFlags::NOFOLLOW,
            rustix::fs::Mode::empty(),
        )
        .map_err(internal)?,
    );
    let mut components = relative.components().peekable();
    while let Some(component) = components.next() {
        let name = component.as_os_str();
        if components.peek().is_some() {
            directory = File::from(
                rustix::fs::openat(
                    &directory,
                    name,
                    rustix::fs::OFlags::RDONLY
                        | rustix::fs::OFlags::DIRECTORY
                        | rustix::fs::OFlags::NOFOLLOW,
                    rustix::fs::Mode::empty(),
                )
                .map_err(internal)?,
            );
        } else {
            let mut file = File::from(
                rustix::fs::openat(
                    &directory,
                    name,
                    rustix::fs::OFlags::RDONLY
                        | rustix::fs::OFlags::NOFOLLOW
                        | rustix::fs::OFlags::NONBLOCK
                        | rustix::fs::OFlags::CLOEXEC,
                    rustix::fs::Mode::empty(),
                )
                .map_err(internal)?,
            );
            if !file.metadata().map_err(internal)?.file_type().is_file() {
                return Err(ApiError::BadRequest(
                    "eval manifest is not a regular file".to_string(),
                ));
            }
            let mut content = String::new();
            file.read_to_string(&mut content).map_err(internal)?;
            return Ok(content);
        }
    }
    Err(ApiError::BadRequest(
        "eval manifest path is empty".to_string(),
    ))
}

#[cfg(unix)]
struct EvalCandidate {
    modified: std::time::SystemTime,
    report: Option<EvalRunReport>,
    marker: Option<EvalRunMarker>,
    label: String,
}

#[cfg(unix)]
fn open_optional_entry(
    parent: &File,
    name: &str,
    directory: bool,
) -> Result<Option<File>, ApiError> {
    let mut flags =
        rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::NOFOLLOW | rustix::fs::OFlags::CLOEXEC;
    if directory {
        flags |= rustix::fs::OFlags::DIRECTORY;
    } else {
        flags |= rustix::fs::OFlags::NONBLOCK;
    }
    match rustix::fs::openat(parent, name, flags, rustix::fs::Mode::empty()) {
        Ok(file) => {
            let file = File::from(file);
            if !directory && !file.metadata().map_err(internal)?.file_type().is_file() {
                return Err(ApiError::BadRequest(format!(
                    "eval entry {name} is not a regular file"
                )));
            }
            Ok(Some(file))
        }
        Err(rustix::io::Errno::NOENT) => Ok(None),
        Err(error) => Err(ApiError::BadRequest(format!(
            "unsafe eval entry {name}: {error}"
        ))),
    }
}

#[cfg(unix)]
fn scan_eval_candidates(root: &Path) -> Result<(Vec<EvalCandidate>, Vec<String>, bool), ApiError> {
    let root_directory = File::from(
        rustix::fs::open(
            root,
            rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::DIRECTORY
                | rustix::fs::OFlags::NOFOLLOW,
            rustix::fs::Mode::empty(),
        )
        .map_err(internal)?,
    );
    let Some(artifacts) = open_optional_entry(&root_directory, "artifacts", true)? else {
        return Ok((Vec::new(), Vec::new(), false));
    };
    let Some(evals) = open_optional_entry(&artifacts, "eval", true)? else {
        return Ok((Vec::new(), Vec::new(), false));
    };
    let mut candidates = Vec::new();
    let mut errors = Vec::new();
    let mut unresolved = false;
    for item in rustix::fs::Dir::read_from(&evals).map_err(internal)? {
        let item = item.map_err(internal)?;
        let name = item.file_name();
        if name.to_bytes() == b"." || name.to_bytes() == b".." {
            continue;
        }
        let label = format!(
            "{}/artifacts/eval/{}",
            root.display(),
            name.to_string_lossy()
        );
        let run_directory = match rustix::fs::openat(
            &evals,
            name,
            rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::DIRECTORY
                | rustix::fs::OFlags::NOFOLLOW,
            rustix::fs::Mode::empty(),
        ) {
            Ok(directory) => File::from(directory),
            Err(rustix::io::Errno::NOENT | rustix::io::Errno::NOTDIR) => continue,
            Err(error) => {
                errors.push(format!("{label}: unsafe run directory: {error}"));
                unresolved = true;
                continue;
            }
        };
        let report_file = match open_optional_entry(&run_directory, "report.json", false) {
            Ok(report) => report,
            Err(error) => {
                errors.push(format!("{label}: {error}"));
                unresolved = true;
                continue;
            }
        };
        let marker_file = match open_optional_entry(&run_directory, "run-state.json", false) {
            Ok(marker) => marker,
            Err(error) => {
                errors.push(format!("{label}: {error}"));
                unresolved = true;
                continue;
            }
        };
        let modified = report_file
            .as_ref()
            .or(marker_file.as_ref())
            .map(|file| file.metadata().and_then(|metadata| metadata.modified()))
            .transpose()
            .map_err(internal)?;
        let Some(modified) = modified else { continue };
        let marker: Option<EvalRunMarker> = match marker_file {
            Some(file) => match serde_json::from_reader(file) {
                Ok(marker) => Some(marker),
                Err(error) => {
                    errors.push(format!("{label}/run-state.json: {error}"));
                    unresolved = true;
                    None
                }
            },
            None => None,
        };
        let report: Option<EvalRunReport> = match report_file {
            Some(file) => match serde_json::from_reader(file) {
                Ok(report) => Some(report),
                Err(error) => {
                    errors.push(format!("{label}/report.json: {error}"));
                    None
                }
            },
            None => None,
        };
        if let Some(marker) = marker.as_ref() {
            match marker.status.as_str() {
                "running" => match report.as_ref() {
                    Some(saved) if saved.run_id == marker.run_id => {}
                    Some(_) => {
                        errors.push(format!("{label}/report.json: run ID differs from marker"));
                        unresolved = true;
                    }
                    None => unresolved = true,
                },
                "failed" => {}
                _ => {
                    errors.push(format!("{label}/run-state.json: invalid status"));
                    unresolved = true;
                }
            }
        }
        if report.is_none() && marker.is_none() {
            continue;
        }
        candidates.push(EvalCandidate {
            modified,
            report,
            marker,
            label,
        });
        candidates.sort_by_key(|candidate| std::cmp::Reverse(candidate.modified));
        candidates.truncate(MAX_EVAL_RUNS);
    }
    Ok((candidates, errors, unresolved))
}

#[cfg(unix)]
#[derive(Debug)]
struct EvalReportOutput {
    directory_path: PathBuf,
    directory: File,
}

#[cfg(unix)]
impl EvalReportOutput {
    fn create(root: &Path, run_id: &str) -> Result<Self, ApiError> {
        let root_directory = File::from(
            rustix::fs::open(
                root,
                rustix::fs::OFlags::RDONLY
                    | rustix::fs::OFlags::DIRECTORY
                    | rustix::fs::OFlags::NOFOLLOW
                    | rustix::fs::OFlags::CLOEXEC,
                rustix::fs::Mode::empty(),
            )
            .map_err(internal)?,
        );
        let artifacts = create_or_open_child(&root_directory, "artifacts")?;
        let evals = create_or_open_child(&artifacts, "eval")?;
        rustix::fs::mkdirat(&evals, run_id, rustix::fs::Mode::RWXU).map_err(internal)?;
        let directory = open_child(&evals, run_id)?;
        Ok(Self {
            directory_path: root.join("artifacts/eval").join(run_id),
            directory,
        })
    }

    fn write(&self, report: &EvalRunReport) -> Result<(), ApiError> {
        let bytes = serde_json::to_vec_pretty(report).map_err(internal)?;
        self.publish("report.json", &bytes)
    }

    fn write_marker(&self, marker: &EvalRunMarker) -> Result<(), ApiError> {
        let bytes = serde_json::to_vec(marker).map_err(internal)?;
        self.publish("run-state.json", &bytes)
    }

    fn publish(&self, name: &str, bytes: &[u8]) -> Result<(), ApiError> {
        let temporary = format!("{name}.{}.tmp", uuid::Uuid::new_v4());

        let result = (|| {
            let file = rustix::fs::openat(
                &self.directory,
                temporary.as_str(),
                rustix::fs::OFlags::WRONLY
                    | rustix::fs::OFlags::CREATE
                    | rustix::fs::OFlags::EXCL
                    | rustix::fs::OFlags::NOFOLLOW
                    | rustix::fs::OFlags::CLOEXEC,
                rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
            )
            .map_err(internal)?;
            let mut file = File::from(file);
            file.write_all(bytes).map_err(internal)?;
            file.sync_all().map_err(internal)?;
            rustix::fs::renameat(&self.directory, temporary.as_str(), &self.directory, name)
                .map_err(internal)
        })();

        if result.is_err() {
            let cleanup = rustix::fs::unlinkat(
                &self.directory,
                temporary.as_str(),
                rustix::fs::AtFlags::empty(),
            )
            .map_err(std::io::Error::from);
            if let Err(error) = cleanup {
                if error.kind() != std::io::ErrorKind::NotFound {
                    tracing::error!(path = %self.directory_path.display(), %error, "failed to remove incomplete eval report");
                }
            }
        }
        result
    }
}

fn entry(
    report: &EvalRunReport,
    reported_at: DateTime<Utc>,
    status: &str,
) -> Result<EvalRunEntry, ApiError> {
    Ok(EvalRunEntry {
        run_id: report.run_id.clone(),
        suite: report.suite.clone(),
        status: status.to_string(),
        report: serde_json::to_value(report).map_err(internal)?,
        reported_at: reported_at.to_rfc3339(),
        error: None,
    })
}

fn internal(error: impl std::fmt::Display) -> ApiError {
    ApiError::Internal(error.to_string())
}

#[cfg(all(test, unix))]
#[path = "eval_runs_tests.rs"]
mod tests;
