use crate::http::background::loop_health::LoopSnapshot;
use crate::http::AppState;

pub(super) fn load_workflow_config(
    state: &AppState,
) -> Option<harness_core::config::workflow::WorkflowConfig> {
    state.core.workflow_runtime_store.as_ref()?;
    match harness_core::config::workflow::load_workflow_config(&state.core.project_root) {
        Ok(config) => {
            state.background_loops.clear_config_failure();
            Some(config)
        }
        Err(error) => {
            tracing::error!(
                project_root = %state.core.project_root.display(),
                error = %error,
                "operator monitor cannot calculate config-dependent projections from malformed WORKFLOW.md"
            );
            state
                .background_loops
                .record_config_failure("workflow_watchdog", &error.to_string());
            None
        }
    }
}

pub(super) fn append_background_loop_degradations(
    subsystems: &mut Vec<&'static str>,
    loop_snapshots: &[LoopSnapshot],
    has_config_parse_failure: bool,
) {
    for loop_snapshot in loop_snapshots {
        if loop_snapshot.stale {
            subsystems.push(match loop_snapshot.name {
                "orphan_schema_reaper" => "orphan_schema_reaper_stale",
                "workflow_watchdog" => "workflow_watchdog_stale",
                "runtime_retention" => "runtime_retention_stale",
                "task_retention" => "task_retention_stale",
                _ => {
                    // Guards against future loop names missing a mapping
                    // without leaking a string per snapshot; the precise name
                    // is in `background_loops`.
                    "background_loop_stale"
                }
            });
        }
    }
    if has_config_parse_failure {
        subsystems.push("workflow_config_parse_failure");
    }
}

pub(super) fn operator_health_status(
    degraded_subsystems: &[&'static str],
    runtime_log_state: &str,
    runtime_state_dirty: bool,
    isolation_degraded: bool,
) -> &'static str {
    if degraded_subsystems.is_empty()
        && runtime_log_state != "degraded"
        && !runtime_state_dirty
        && !isolation_degraded
    {
        "ok"
    } else {
        "degraded"
    }
}
