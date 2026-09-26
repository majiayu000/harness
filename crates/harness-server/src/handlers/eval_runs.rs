use crate::handlers::{validate_file_in_root, validate_project_root};
use crate::http::api_error::ApiError;
use crate::http::rest_contract::{ContractJson, ContractQuery};
use crate::http::AppState;
use axum::extract::State;
use chrono::{DateTime, Utc};
use harness_protocol::rest::{
    EvalRunEntry, EvalRunListQuery, EvalRunListResponse, EvalRunRequest, EvalRunResponse,
};
use harness_workflow::runtime::{
    eval_report_dry_run, execute_manifest, parse_benchmark_manifest_str, EvalEventPersistenceError,
    EvalExecuteConfig, EvalRunReport,
};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub(crate) async fn list_eval_runs(
    State(state): State<Arc<AppState>>,
    ContractQuery(query): ContractQuery<EvalRunListQuery>,
) -> Result<ContractJson<EvalRunListResponse>, ApiError> {
    let root = project_root(&state, &query.project_root)?;
    let directory = root.join("artifacts/eval");
    if !directory.exists() {
        return Ok(ContractJson(EvalRunListResponse { runs: Vec::new() }));
    }
    let directory = validate_file_in_root(&directory, &root).map_err(ApiError::BadRequest)?;
    if !directory.is_dir() {
        return Err(ApiError::BadRequest(
            "eval report path is not a directory".to_string(),
        ));
    }
    let mut reports = Vec::new();
    for entry in fs::read_dir(&directory).map_err(internal)? {
        let entry = entry.map_err(internal)?;
        let path = entry.path().join("report.json");
        if !path.is_file() {
            continue;
        }
        let path = validate_file_in_root(&path, &directory).map_err(ApiError::BadRequest)?;
        let modified = fs::metadata(&path)
            .and_then(|metadata| metadata.modified())
            .map_err(internal)?;
        reports.push((modified, path));
    }
    reports.sort_by_key(|(modified, _)| std::cmp::Reverse(*modified));
    let runs = reports
        .into_iter()
        .take(20)
        .map(|(modified, path)| {
            let report: EvalRunReport =
                serde_json::from_slice(&fs::read(path).map_err(internal)?).map_err(internal)?;
            entry(&report, DateTime::<Utc>::from(modified))
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(ContractJson(EvalRunListResponse { runs }))
}

pub(crate) async fn run_eval(
    State(state): State<Arc<AppState>>,
    ContractJson(request): ContractJson<EvalRunRequest>,
) -> Result<ContractJson<EvalRunResponse>, ApiError> {
    let root = project_root(&state, &request.project_root)?;
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
            run: entry(&report, Utc::now())?,
        }));
    }

    let store = state.workflow_runtime_store()?.clone();
    let events = state.observability.events.clone();
    let directory = create_eval_directory(&root, &run_id)?;
    let report_path = directory.join("report.json");
    let project_id = root.to_string_lossy().into_owned();
    let task = tokio::spawn(async move {
        let result: Result<ContractJson<EvalRunResponse>, ApiError> = async {
            let config = EvalExecuteConfig::new(run_id.clone(), project_id.clone(), 3);
            let report = match execute_manifest(&store, &events, &manifest, config).await {
                Ok(report) => report,
                Err(error) => {
                    if let Some(partial) = error.downcast_ref::<EvalEventPersistenceError>() {
                        write_report(&report_path, partial.report())?;
                    }
                    return Err(ApiError::Internal(error.to_string()));
                }
            };
            write_report(&report_path, &report)?;
            entry(&report, Utc::now()).map(|run| ContractJson(EvalRunResponse { run }))
        }
        .await;
        if let Err(error) = &result {
            tracing::error!(run_id = %run_id, project = %project_id, %error, "console eval run failed");
        }
        result
    });
    task.await
        .map_err(|error| ApiError::Internal(format!("eval task failed: {error}")))?
}

fn project_root(state: &AppState, requested: &str) -> Result<PathBuf, ApiError> {
    validate_project_root(Path::new(requested), &state.core.home_dir).map_err(ApiError::BadRequest)
}

fn create_eval_directory(root: &Path, run_id: &str) -> Result<PathBuf, ApiError> {
    let artifacts = root.join("artifacts");
    if !artifacts.exists() {
        fs::create_dir(&artifacts).map_err(internal)?;
    }
    let artifacts = validate_file_in_root(&artifacts, root).map_err(ApiError::BadRequest)?;
    let evals = artifacts.join("eval");
    if !evals.exists() {
        fs::create_dir(&evals).map_err(internal)?;
    }
    let evals = validate_file_in_root(&evals, root).map_err(ApiError::BadRequest)?;
    let directory = evals.join(run_id);
    fs::create_dir(&directory).map_err(internal)?;
    Ok(directory)
}

fn write_report(path: &Path, report: &EvalRunReport) -> Result<(), ApiError> {
    let bytes = serde_json::to_vec_pretty(report).map_err(internal)?;
    let temporary = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .map_err(internal)?;
        file.write_all(&bytes).map_err(internal)?;
        file.sync_all().map_err(internal)?;
        fs::rename(&temporary, path).map_err(internal)
    })();
    if result.is_err() {
        if let Err(error) = fs::remove_file(&temporary) {
            if error.kind() != std::io::ErrorKind::NotFound {
                tracing::error!(path = %temporary.display(), %error, "failed to remove incomplete eval report");
            }
        }
    }
    result
}

fn entry(report: &EvalRunReport, reported_at: DateTime<Utc>) -> Result<EvalRunEntry, ApiError> {
    Ok(EvalRunEntry {
        report: serde_json::to_value(report).map_err(internal)?,
        reported_at: reported_at.to_rfc3339(),
    })
}

fn internal(error: impl std::fmt::Display) -> ApiError {
    ApiError::Internal(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers::{db_tests_enabled, make_test_state, tempdir_in_home, HOME_LOCK};

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
        let error = create_eval_directory(&project, "run-1").expect_err("outside output rejected");
        assert_eq!(error.status(), axum::http::StatusCode::BAD_REQUEST);
        assert!(!outside.join("eval").exists());
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
        fs::create_dir_all(&report_directory)?;
        fs::write(report_directory.join("report.incomplete.tmp"), b"{")?;
        let query = EvalRunListQuery {
            project_root: request.project_root.clone(),
        };
        let before_publish =
            list_eval_runs(State(state.clone()), ContractQuery(query.clone())).await?;
        assert!(before_publish.0.runs.is_empty());
        let report: EvalRunReport = serde_json::from_value(response.0.run.report)?;
        write_report(&report_directory.join("report.json"), &report)?;
        let listed = list_eval_runs(State(state.clone()), ContractQuery(query)).await?;
        assert_eq!(listed.0.runs.len(), 1);
        assert_eq!(listed.0.runs[0].report["suite"], "eval-isolation-fixture");

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
