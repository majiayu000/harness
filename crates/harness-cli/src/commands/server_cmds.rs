use harness_core::config::HarnessConfig;
use std::path::PathBuf;

use super::LoggingBootstrap;

pub fn log_runtime_log_status(
    #[cfg_attr(not(feature = "server"), allow(unused_variables))] bootstrap: &LoggingBootstrap,
) {
    #[cfg(feature = "server")]
    match bootstrap.runtime_logs.state {
        harness_server::server::RuntimeLogState::Enabled => {
            if let Some(path) = bootstrap.runtime_logs.active_path.as_ref() {
                tracing::info!(
                    path = %path.display(),
                    retention_days = bootstrap.runtime_logs.retention_days,
                    retention_max_files = bootstrap.runtime_logs.retention_max_files,
                    "runtime logs persisted to file"
                );
            }
            for warning in &bootstrap.retention_warnings {
                tracing::warn!(warning = %warning, "runtime log retention cleanup skipped an entry");
            }
        }
        harness_server::server::RuntimeLogState::Degraded => {
            tracing::warn!(
                path_hint = bootstrap
                    .runtime_logs
                    .path_hint
                    .as_deref()
                    .unwrap_or("logs"),
                retention_days = bootstrap.runtime_logs.retention_days,
                retention_max_files = bootstrap.runtime_logs.retention_max_files,
                error = bootstrap
                    .setup_warning
                    .as_deref()
                    .unwrap_or("unknown setup error"),
                "runtime log persistence unavailable; continuing with console logging only"
            );
        }
        harness_server::server::RuntimeLogState::Disabled => {}
    }
}

#[cfg(not(feature = "server"))]
fn missing_server_feature(command: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "`harness {command}` requires the `server` cargo feature so the CLI can link harness-server. Rebuild with `cargo build -p harness-cli --features server`."
    )
}

pub async fn run_serve(
    config: HarnessConfig,
    transport: Option<String>,
    port: Option<u16>,
    project_root: Option<PathBuf>,
    projects: Vec<String>,
    default_project: Option<String>,
    logging: &LoggingBootstrap,
) -> anyhow::Result<()> {
    #[cfg(feature = "server")]
    {
        super::serve::run(
            config,
            transport,
            port,
            project_root,
            projects,
            default_project,
            logging.runtime_logs.clone(),
        )
        .await
    }
    #[cfg(not(feature = "server"))]
    {
        let _ = (
            config,
            transport,
            port,
            project_root,
            projects,
            default_project,
            logging,
        );
        Err(missing_server_feature("serve"))
    }
}

pub async fn run_reconcile(
    dry_run: bool,
    project: Option<PathBuf>,
    config: &HarnessConfig,
) -> anyhow::Result<()> {
    #[cfg(feature = "server")]
    {
        super::reconcile::run(dry_run, project, config).await
    }
    #[cfg(not(feature = "server"))]
    {
        let _ = (dry_run, project, config);
        Err(missing_server_feature("reconcile"))
    }
}

#[cfg(all(test, not(feature = "server")))]
mod tests {
    use super::*;
    use harness_core::config::HarnessConfig;

    #[tokio::test]
    async fn serve_without_server_feature_explains_rebuild() {
        let logging = LoggingBootstrap {
            runtime_log_file: None,
        };
        let err = run_serve(
            HarnessConfig::default(),
            None,
            None,
            None,
            Vec::new(),
            None,
            &logging,
        )
        .await
        .expect_err("serve should fail without the server feature");
        let message = err.to_string();
        assert!(
            message.contains("`harness serve` requires the `server` cargo feature"),
            "{message}"
        );
    }

    #[tokio::test]
    async fn reconcile_without_server_feature_explains_rebuild() {
        let err = run_reconcile(true, None, &HarnessConfig::default())
            .await
            .expect_err("reconcile should fail without the server feature");
        let message = err.to_string();
        assert!(
            message.contains("`harness reconcile` requires the `server` cargo feature"),
            "{message}"
        );
    }
}
