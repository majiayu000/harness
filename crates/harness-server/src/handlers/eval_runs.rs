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
use std::fs;
#[cfg(unix)]
use std::fs::File;
#[cfg(unix)]
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
#[cfg(unix)]
use std::sync::OnceLock;

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

pub(crate) async fn list_eval_runs(
    State(state): State<Arc<AppState>>,
    ContractQuery(query): ContractQuery<EvalRunListQuery>,
) -> Result<ContractJson<EvalRunListResponse>, ApiError> {
    let root = project_root(&state, &query.project_root).await?;
    let directory = root.join("artifacts/eval");
    if !directory.exists() {
        return Ok(ContractJson(EvalRunListResponse {
            runs: Vec::new(),
            errors: Vec::new(),
        }));
    }
    let directory = validate_file_in_root(&directory, &root).map_err(ApiError::BadRequest)?;
    if !directory.is_dir() {
        return Err(ApiError::BadRequest(
            "eval report path is not a directory".to_string(),
        ));
    }
    let mut reports = Vec::new();
    let mut errors = Vec::new();
    for entry in fs::read_dir(&directory).map_err(internal)? {
        let entry = entry.map_err(internal)?;
        let report_path = entry.path().join("report.json");
        let marker_path = entry.path().join("run-state.json");
        let (path, is_marker) = if report_path.is_file() {
            (report_path, false)
        } else if marker_path.is_file() {
            (marker_path, true)
        } else {
            continue;
        };
        let path = match validate_file_in_root(&path, &directory) {
            Ok(path) => path,
            Err(error) => {
                errors.push(error);
                continue;
            }
        };
        let modified = match fs::metadata(&path).and_then(|metadata| metadata.modified()) {
            Ok(modified) => modified,
            Err(error) => {
                errors.push(format!("{}: {error}", path.display()));
                continue;
            }
        };
        reports.push((modified, path, is_marker));
    }
    reports.sort_by_key(|(modified, _, _)| std::cmp::Reverse(*modified));
    let mut runs = Vec::new();
    for (modified, path, is_marker) in reports {
        if runs.len() == 20 {
            break;
        }
        let result = if is_marker {
            read_marker_entry(&path, &root)
        } else {
            read_report_entry(&path, modified)
        };
        match result {
            Ok(run) => runs.push(run),
            Err(error) => errors.push(format!("{}: {error}", path.display())),
        }
    }
    for error in &errors {
        tracing::error!(%error, "console eval report unavailable");
    }
    Ok(ContractJson(EvalRunListResponse { runs, errors }))
}

pub(crate) async fn run_eval(
    State(state): State<Arc<AppState>>,
    ContractJson(request): ContractJson<EvalRunRequest>,
) -> Result<ContractJson<EvalRunResponse>, ApiError> {
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
        let store = state.workflow_runtime_store()?.clone();
        let events = state.observability.events.clone();
        let active = ActiveEvalProject::acquire(&root, &run_id)?;
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

fn read_report_entry(
    path: &Path,
    modified: std::time::SystemTime,
) -> Result<EvalRunEntry, ApiError> {
    let report: EvalRunReport =
        serde_json::from_slice(&fs::read(path).map_err(internal)?).map_err(internal)?;
    entry(&report, DateTime::<Utc>::from(modified), "completed")
}

fn read_marker_entry(path: &Path, root: &Path) -> Result<EvalRunEntry, ApiError> {
    let marker: EvalRunMarker =
        serde_json::from_slice(&fs::read(path).map_err(internal)?).map_err(internal)?;
    let status = match marker.status.as_str() {
        "failed" => "failed",
        "running"
            if marker.pid == std::process::id() && active_eval_project(root, &marker.run_id) =>
        {
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

fn active_eval_project(root: &Path, run_id: &str) -> bool {
    #[cfg(unix)]
    {
        ACTIVE_EVAL_PROJECTS.get().is_some_and(|active| {
            active
                .get(root)
                .is_some_and(|entry| entry.value() == run_id)
        })
    }
    #[cfg(not(unix))]
    {
        let _ = (root, run_id);
        false
    }
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
mod tests {
    use super::*;
    use crate::test_helpers::{db_tests_enabled, make_test_state, tempdir_in_home, HOME_LOCK};
    use chrono::TimeZone;

    #[test]
    fn eval_dispatch_respects_maintenance_window() {
        let mut maintenance = harness_core::config::maintenance::MaintenanceWindowConfig::default();
        maintenance.enabled = true;
        maintenance.timezone = "UTC".to_string();
        maintenance.quiet_window_start = chrono::NaiveTime::from_hms_opt(0, 0, 0).unwrap();
        maintenance.quiet_window_end = chrono::NaiveTime::from_hms_opt(23, 0, 0).unwrap();
        let now = Utc
            .with_ymd_and_hms(2026, 9, 27, 12, 0, 0)
            .single()
            .unwrap();
        let error = reject_during_maintenance(&maintenance, now)
            .expect_err("maintenance must stop eval dispatch");
        assert_eq!(error.status(), axum::http::StatusCode::SERVICE_UNAVAILABLE);
        maintenance.enabled = false;
        assert!(reject_during_maintenance(&maintenance, now).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn eval_output_rejects_artifacts_symlink_outside_project() -> anyhow::Result<()> {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir()?;
        let project = dir.path().join("project");
        let outside = dir.path().join("outside");
        fs::create_dir(&project)?;
        fs::create_dir(&outside)?;
        symlink(&outside, project.join("artifacts"))?;
        let error = EvalReportOutput::create(&project.canonicalize()?, "run-1")
            .expect_err("outside output rejected");
        assert_eq!(error.status(), axum::http::StatusCode::BAD_REQUEST);
        assert!(!outside.join("eval").exists());
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn eval_output_cannot_follow_late_run_directory_symlink() -> anyhow::Result<()> {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir()?;
        let project = dir.path().join("project");
        let outside = dir.path().join("outside");
        fs::create_dir(&project)?;
        fs::create_dir(&outside)?;
        let project = project.canonicalize()?;
        let output = EvalReportOutput::create(&project, "run-1")?;
        let run_directory = project.join("artifacts/eval/run-1");
        let moved = project.join("artifacts/eval/moved-run");
        fs::rename(&run_directory, &moved)?;
        symlink(&outside, &run_directory)?;

        let manifest = parse_benchmark_manifest_str(include_str!(
            "../../../../evals/benchmarks/eval-isolation-fixture.toml"
        ))?;
        let report = eval_report_dry_run(&manifest, "run-1", 3)?;
        output.write(&report)?;
        assert!(!outside.join("report.json").exists());
        assert!(moved.join("report.json").exists());
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn eval_child_creation_stays_in_pinned_parent_directory() -> anyhow::Result<()> {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir()?;
        let project = dir.path().join("project");
        let outside = dir.path().join("outside");
        fs::create_dir(&project)?;
        fs::create_dir(&outside)?;
        let project = project.canonicalize()?;
        let root = File::from(rustix::fs::open(
            &project,
            rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::DIRECTORY
                | rustix::fs::OFlags::NOFOLLOW,
            rustix::fs::Mode::empty(),
        )?);
        let artifacts = create_or_open_child(&root, "artifacts")?;
        let evals = create_or_open_child(&artifacts, "eval")?;
        let moved = project.join("artifacts/eval-moved");
        fs::rename(project.join("artifacts/eval"), &moved)?;
        symlink(&outside, project.join("artifacts/eval"))?;
        rustix::fs::mkdirat(&evals, "run-1", rustix::fs::Mode::RWXU)?;
        assert!(moved.join("run-1").is_dir());
        assert!(!outside.join("run-1").exists());
        Ok(())
    }

    #[tokio::test]
    async fn dry_run_uses_project_manifest_and_rejects_escape() -> anyhow::Result<()> {
        if !db_tests_enabled().await {
            return Ok(());
        }
        let _lock = HOME_LOCK.lock().await;
        let dir = tempdir_in_home("eval-run-dry-")?;
        let mut state = Arc::new(make_test_state(dir.path()).await?);
        fs::write(
            dir.path().join("manifest.toml"),
            include_str!("../../../../evals/benchmarks/eval-isolation-fixture.toml"),
        )?;
        let request = EvalRunRequest {
            project_root: dir.path().to_string_lossy().into_owned(),
            manifest_path: "manifest.toml".to_string(),
            dry_run: true,
        };
        let response = run_eval(State(state.clone()), ContractJson(request.clone())).await?;
        assert_eq!(response.0.run.report["metrics"]["total_cases"], 1);
        assert!(response.0.run.report["run_id"]
            .as_str()
            .is_some_and(|id| id.starts_with("console-")));
        let report_directory = dir.path().join("artifacts/eval/run-1");
        let output = EvalReportOutput::create(&dir.path().canonicalize()?, "run-1")?;
        fs::write(report_directory.join("report.incomplete.tmp"), b"{")?;
        let query = EvalRunListQuery {
            project_root: request.project_root.clone(),
        };
        let before_publish =
            list_eval_runs(State(state.clone()), ContractQuery(query.clone())).await?;
        assert!(before_publish.0.runs.is_empty());
        let report: EvalRunReport = serde_json::from_value(response.0.run.report)?;
        output.write(&report)?;
        let listed = list_eval_runs(State(state.clone()), ContractQuery(query)).await?;
        assert_eq!(listed.0.runs.len(), 1);
        assert_eq!(listed.0.runs[0].report["suite"], "eval-isolation-fixture");
        let incomplete_directory = dir.path().join("artifacts/eval/run-2");
        fs::create_dir_all(&incomplete_directory)?;
        fs::write(incomplete_directory.join("report.json"), b"")?;
        let partial = list_eval_runs(
            State(state.clone()),
            ContractQuery(EvalRunListQuery {
                project_root: request.project_root.clone(),
            }),
        )
        .await?;
        assert_eq!(partial.0.runs.len(), 1);
        assert_eq!(partial.0.errors.len(), 1);

        let active_root = dir.path().canonicalize()?;
        let active = ActiveEvalProject::acquire(&active_root, "run-3")?;
        assert!(ActiveEvalProject::acquire(&active_root, "run-4").is_err());
        let active_output = EvalReportOutput::create(&active_root, "run-3")?;
        active_output.write_marker(&EvalRunMarker {
            run_id: "run-3".to_string(),
            suite: "eval-isolation-fixture".to_string(),
            started_at: Utc::now().to_rfc3339(),
            pid: std::process::id(),
            status: "running".to_string(),
            error: None,
        })?;
        let active_list = list_eval_runs(
            State(state.clone()),
            ContractQuery(EvalRunListQuery {
                project_root: request.project_root.clone(),
            }),
        )
        .await?;
        let active_row = active_list
            .0
            .runs
            .iter()
            .find(|run| run.run_id == "run-3")
            .expect("running eval is listed");
        assert_eq!(active_row.status, "running");
        assert!(active_row.report.is_null());
        drop(active);
        let interrupted = list_eval_runs(
            State(state.clone()),
            ContractQuery(EvalRunListQuery {
                project_root: request.project_root.clone(),
            }),
        )
        .await?;
        assert_eq!(
            interrupted
                .0
                .runs
                .iter()
                .find(|run| run.run_id == "run-3")
                .expect("interrupted eval is listed")
                .status,
            "interrupted"
        );
        let next_active = ActiveEvalProject::acquire(&active_root, "run-4")?;
        let still_interrupted = list_eval_runs(
            State(state.clone()),
            ContractQuery(EvalRunListQuery {
                project_root: request.project_root.clone(),
            }),
        )
        .await?;
        assert_eq!(
            still_interrupted
                .0
                .runs
                .iter()
                .find(|run| run.run_id == "run-3")
                .expect("older marker remains listed")
                .status,
            "interrupted"
        );
        drop(next_active);

        let outside_home = tempfile::tempdir()?;
        let external_root = outside_home.path().canonicalize()?;
        assert!(!external_root.starts_with(&state.core.home_dir));
        fs::create_dir(external_root.join(".git"))?;
        fs::write(
            external_root.join("manifest.toml"),
            include_str!("../../../../evals/benchmarks/eval-isolation-fixture.toml"),
        )?;
        state
            .project_svc
            .register(crate::project_registry::Project {
                id: "external-eval-project".to_string(),
                root: external_root.clone(),
                name: None,
                max_concurrent: None,
                default_agent: None,
                active: true,
                created_at: Utc::now().to_rfc3339(),
            })
            .await?;
        let external_response = run_eval(
            State(state.clone()),
            ContractJson(EvalRunRequest {
                project_root: external_root.to_string_lossy().into_owned(),
                manifest_path: "manifest.toml".to_string(),
                dry_run: true,
            }),
        )
        .await?;
        assert_eq!(external_response.0.run.report["metrics"]["total_cases"], 1);

        let escaped = EvalRunRequest {
            manifest_path: "../outside.toml".to_string(),
            ..request.clone()
        };
        let error = run_eval(State(state.clone()), ContractJson(escaped))
            .await
            .expect_err("outside manifest should be rejected");
        assert_eq!(error.status(), axum::http::StatusCode::BAD_REQUEST);

        Arc::get_mut(&mut state)
            .expect("only the test owns the state")
            .core
            .workflow_runtime_store = None;
        let execute = EvalRunRequest {
            dry_run: false,
            ..request
        };
        let error = run_eval(State(state), ContractJson(execute))
            .await
            .expect_err("execute requires the workflow runtime store");
        assert_eq!(error.status(), axum::http::StatusCode::SERVICE_UNAVAILABLE);
        Ok(())
    }
}
