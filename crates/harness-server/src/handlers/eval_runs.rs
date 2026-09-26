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
use std::path::{Path, PathBuf};
use std::sync::Arc;
#[cfg(unix)]
use std::sync::OnceLock;

#[cfg(unix)]
#[path = "eval_runs_storage.rs"]
mod storage;
#[cfg(all(test, unix))]
use storage::create_or_open_child;
#[cfg(unix)]
use storage::{read_project_file, scan_eval_candidates, EvalReportOutput};

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
    use std::fs::File;

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

    #[cfg(unix)]
    #[test]
    fn eval_report_read_stays_pinned_after_run_directory_replacement() -> anyhow::Result<()> {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir()?;
        let project = dir.path().join("project");
        let outside = dir.path().join("outside");
        fs::create_dir(&project)?;
        fs::create_dir(&outside)?;
        fs::write(outside.join("report.json"), b"not a report")?;
        let project = project.canonicalize()?;
        let output = EvalReportOutput::create(&project, "run-1")?;
        let manifest = parse_benchmark_manifest_str(include_str!(
            "../../../../evals/benchmarks/eval-isolation-fixture.toml"
        ))?;
        let report = eval_report_dry_run(&manifest, "original-run", 3)?;
        output.write(&report)?;
        let (mut candidates, errors, _) = scan_eval_candidates(&project)?;
        assert!(errors.is_empty());
        let run_directory = project.join("artifacts/eval/run-1");
        fs::rename(&run_directory, project.join("artifacts/eval/moved-run"))?;
        symlink(&outside, &run_directory)?;
        let candidate = candidates.pop().expect("report was scanned");
        let read = read_report_entry(
            candidate.report.as_ref().expect("pinned report"),
            candidate.modified,
            candidate.marker.as_ref(),
        )?;
        assert_eq!(read.run_id, "original-run");
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn eval_file_reads_reject_fifo_without_blocking() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let project = dir.path().join("project");
        fs::create_dir(&project)?;
        let project = project.canonicalize()?;
        EvalReportOutput::create(&project, "run-1")?;
        let report_fifo = project.join("artifacts/eval/run-1/report.json");
        let manifest_fifo = project.join("manifest.toml");
        for path in [&report_fifo, &manifest_fifo] {
            assert!(std::process::Command::new("mkfifo")
                .arg(path)
                .status()?
                .success());
        }
        let started = std::time::Instant::now();
        let (_, errors, unresolved) = scan_eval_candidates(&project)?;
        assert!(unresolved);
        assert!(!errors.is_empty());
        assert!(read_project_file(&project, &manifest_fifo).is_err());
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn eval_scan_bounds_handles_and_requires_matching_terminal_report() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let project = dir.path().join("project");
        fs::create_dir(&project)?;
        let project = project.canonicalize()?;
        let manifest = parse_benchmark_manifest_str(include_str!(
            "../../../../evals/benchmarks/eval-isolation-fixture.toml"
        ))?;
        for index in 0..25 {
            let run_id = format!("run-{index}");
            let output = EvalReportOutput::create(&project, &run_id)?;
            output.write(&eval_report_dry_run(&manifest, run_id, 3)?)?;
        }
        let (candidates, errors, unresolved) = scan_eval_candidates(&project)?;
        assert_eq!(candidates.len(), MAX_EVAL_RUNS);
        assert!(errors.is_empty());
        assert!(!unresolved);

        for index in 0..21 {
            let run_id = format!("bad-{index}");
            EvalReportOutput::create(&project, &run_id)?;
            fs::write(
                project
                    .join("artifacts/eval")
                    .join(run_id)
                    .join("report.json"),
                b"",
            )?;
        }
        let (candidates, errors, unresolved) = scan_eval_candidates(&project)?;
        assert_eq!(candidates.len(), MAX_EVAL_RUNS);
        assert!(candidates
            .iter()
            .all(|candidate| candidate.report.is_some()));
        assert!(
            errors.len() >= 21,
            "bad reports must remain visible as errors"
        );
        assert!(!unresolved);

        let empty = EvalReportOutput::create(&project, "run-empty")?;
        empty.write_marker(&EvalRunMarker {
            run_id: "run-empty".to_string(),
            suite: manifest.suite.clone(),
            started_at: Utc::now().to_rfc3339(),
            pid: std::process::id(),
            status: "running".to_string(),
            error: None,
        })?;
        fs::write(project.join("artifacts/eval/run-empty/report.json"), b"")?;
        let mismatch = EvalReportOutput::create(&project, "run-mismatch")?;
        mismatch.write_marker(&EvalRunMarker {
            run_id: "run-mismatch".to_string(),
            suite: manifest.suite.clone(),
            started_at: Utc::now().to_rfc3339(),
            pid: std::process::id(),
            status: "running".to_string(),
            error: None,
        })?;
        mismatch.write(&eval_report_dry_run(&manifest, "different-run", 3)?)?;
        let (candidates, errors, unresolved) = scan_eval_candidates(&project)?;
        assert_eq!(candidates.len(), MAX_EVAL_RUNS);
        assert!(unresolved);
        assert!(errors.iter().any(|error| error.contains("run ID differs")));
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
        let mut report: EvalRunReport = serde_json::from_value(response.0.run.report)?;
        report.outcome = Some(harness_workflow::runtime::EvalRunOutcome::EventPersistenceFailed);
        output.write(&report)?;
        let listed = list_eval_runs(State(state.clone()), ContractQuery(query)).await?;
        assert_eq!(listed.0.runs.len(), 1);
        assert_eq!(listed.0.runs[0].report["suite"], "eval-isolation-fixture");
        output.write_marker(&EvalRunMarker {
            run_id: report.run_id.clone(),
            suite: report.suite.clone(),
            started_at: Utc::now().to_rfc3339(),
            pid: std::process::id(),
            status: "failed".to_string(),
            error: Some("event persistence failed".to_string()),
        })?;
        let failed_with_report = list_eval_runs(
            State(state.clone()),
            ContractQuery(EvalRunListQuery {
                project_root: request.project_root.clone(),
            }),
        )
        .await?;
        assert_eq!(failed_with_report.0.runs[0].status, "failed");
        assert_eq!(
            failed_with_report.0.runs[0].error.as_deref(),
            Some("event persistence failed")
        );
        assert_eq!(
            failed_with_report.0.runs[0].report["suite"],
            "eval-isolation-fixture"
        );
        report.outcome = None;
        output.write(&report)?;
        let repaired = list_eval_runs(
            State(state.clone()),
            ContractQuery(EvalRunListQuery {
                project_root: request.project_root.clone(),
            }),
        )
        .await?;
        assert_eq!(repaired.0.runs[0].status, "completed");
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
        let store = state.workflow_runtime_store()?;
        let active = ActiveEvalProject::acquire(&active_root, "run-3")?;
        assert!(ActiveEvalProject::acquire(&active_root, "run-4").is_err());
        let database_lock = acquire_eval_database_lock(store.pool(), &active_root).await?;
        let other_pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .connect(&crate::test_helpers::test_database_url()?)
            .await?;
        let other_acquired: bool =
            sqlx::query_scalar("SELECT pg_try_advisory_lock(hashtextextended($1, 0))")
                .bind(eval_lock_key(&active_root))
                .fetch_one(&other_pool)
                .await?;
        assert!(
            !other_acquired,
            "another server connection must not acquire the eval lock"
        );
        other_pool.close().await;
        let single_pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .connect(&crate::test_helpers::test_database_url()?)
            .await?;
        let single_pool_lock =
            acquire_eval_database_lock(&single_pool, &active_root.join("single-pool-proof"))
                .await?;
        let can_query: i32 = sqlx::query_scalar("SELECT 1")
            .fetch_one(&single_pool)
            .await?;
        assert_eq!(
            can_query, 1,
            "the runtime pool must remain usable while eval holds its lock"
        );
        single_pool_lock.close().await?;
        single_pool.close().await;
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
        assert!(active_list.0.active);
        assert!(active_list.0.unresolved);
        drop(active);
        database_lock.close().await?;
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
        assert!(!interrupted.0.active);
        assert!(interrupted.0.unresolved);
        let next_active = ActiveEvalProject::acquire(&active_root, "run-4")?;
        let next_database_lock = acquire_eval_database_lock(store.pool(), &active_root).await?;
        let next_output = EvalReportOutput::create(&active_root, "run-4")?;
        next_output.write_marker(&EvalRunMarker {
            run_id: "run-4".to_string(),
            suite: "eval-isolation-fixture".to_string(),
            started_at: Utc::now().to_rfc3339(),
            pid: std::process::id(),
            status: "running".to_string(),
            error: None,
        })?;
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
        assert_eq!(
            still_interrupted
                .0
                .runs
                .iter()
                .find(|run| run.run_id == "run-4")
                .expect("new active marker remains listed")
                .status,
            "running"
        );
        drop(next_active);
        next_database_lock.close().await?;

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

        fs::write(
            dir.path().join("WORKFLOW.md"),
            "---\nruntime_dispatch:\n  enabled: false\nruntime_worker:\n  enabled: true\n---\n",
        )?;
        let execute = EvalRunRequest {
            dry_run: false,
            ..request.clone()
        };
        let error = run_eval(State(state.clone()), ContractJson(execute))
            .await
            .expect_err("disabled workflow loops must reject eval execution");
        assert!(error
            .to_string()
            .contains("dispatch and worker must be enabled"));
        fs::remove_file(dir.path().join("WORKFLOW.md"))?;
        let unresolved_error = run_eval(
            State(state.clone()),
            ContractJson(EvalRunRequest {
                dry_run: false,
                ..request.clone()
            }),
        )
        .await
        .expect_err("an unreconciled run must block replacement dispatch");
        assert_eq!(
            unresolved_error.status(),
            axum::http::StatusCode::BAD_REQUEST
        );
        assert!(unresolved_error
            .to_string()
            .contains("previous Console eval is unresolved"));

        Arc::get_mut(&mut state)
            .expect("only the test owns the state")
            .core
            .workflow_runtime_store = None;
        let uncertain = list_eval_runs(
            State(state.clone()),
            ContractQuery(EvalRunListQuery {
                project_root: request.project_root.clone(),
            }),
        )
        .await?;
        assert!(
            uncertain.0.active,
            "unknown activity must block another dispatch"
        );
        assert!(!uncertain.0.errors.is_empty());
        assert!(uncertain.0.runs.iter().any(|run| run.status == "unknown"));
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
