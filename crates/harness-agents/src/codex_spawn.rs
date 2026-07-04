use harness_core::agent::AgentRequest;
use std::path::{Path, PathBuf};

#[derive(Debug)]
struct ProgramSpawnDiagnostics {
    resolved_path: Option<PathBuf>,
    exists: bool,
    executable: Option<bool>,
}

pub(super) fn resolve_program_for_spawn(program: &Path, current_dir: &Path) -> Option<PathBuf> {
    if program.components().count() == 1 {
        let path = std::env::var_os("PATH")?;
        std::env::split_paths(&path)
            .map(|dir| dir.join(program))
            .find(|candidate| candidate.is_file())
    } else if program.is_absolute() && program.exists() {
        Some(program.to_path_buf())
    } else {
        let candidate = current_dir.join(program);
        candidate.exists().then_some(candidate)
    }
}

#[cfg(unix)]
fn executable_status(path: &Path) -> Option<bool> {
    use std::os::unix::fs::PermissionsExt;

    let metadata = std::fs::metadata(path).ok()?;
    Some(metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn executable_status(path: &Path) -> Option<bool> {
    let metadata = std::fs::metadata(path).ok()?;
    Some(metadata.is_file())
}

fn program_spawn_diagnostics(program: &Path, current_dir: &Path) -> ProgramSpawnDiagnostics {
    let resolved_path = resolve_program_for_spawn(program, current_dir);
    let executable = resolved_path.as_deref().and_then(executable_status);
    ProgramSpawnDiagnostics {
        exists: resolved_path.is_some(),
        resolved_path,
        executable,
    }
}

pub(super) fn log_codex_spawn_attempt(
    program: &Path,
    arg_count: usize,
    req: &AgentRequest,
    engine: harness_sandbox::SandboxEngine,
    mode: &'static str,
) {
    let program_diag = program_spawn_diagnostics(program, &req.project_root);
    let current_dir_exists = req.project_root.exists();
    let current_dir_is_dir = req.project_root.is_dir();
    tracing::debug!(
        agent = "codex",
        mode,
        program = %program.display(),
        program_resolved = program_diag
            .resolved_path
            .as_ref()
            .map(|p| p.display().to_string())
            .as_deref()
            .unwrap_or("<unresolved>"),
        program_exists = program_diag.exists,
        program_executable = ?program_diag.executable,
        current_dir = %req.project_root.display(),
        current_dir_exists,
        current_dir_is_dir,
        phase = ?req.execution_phase,
        sandbox_engine = ?engine,
        arg_count,
        prompt_len = req.prompt.len(),
        env_var_count = req.env_vars.len(),
        "codex spawn prepared"
    );
}

pub(super) fn codex_spawn_failure_message(
    error: &std::io::Error,
    program: &Path,
    req: &AgentRequest,
    engine: harness_sandbox::SandboxEngine,
    mode: &'static str,
) -> String {
    let program_diag = program_spawn_diagnostics(program, &req.project_root);
    let current_dir_exists = req.project_root.exists();
    let current_dir_is_dir = req.project_root.is_dir();
    let resolved_program = program_diag
        .resolved_path
        .as_ref()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "<unresolved>".to_string());

    let message = format!(
        "failed to run codex: {error}; mode={mode}; phase={:?}; program={}; \
         program_resolved={resolved_program}; program_exists={}; program_executable={:?}; \
         current_dir={}; current_dir_exists={current_dir_exists}; \
         current_dir_is_dir={current_dir_is_dir}; sandbox_engine={engine:?}; \
         prompt_len={}; env_var_count={}",
        req.execution_phase,
        program.display(),
        program_diag.exists,
        program_diag.executable,
        req.project_root.display(),
        req.prompt.len(),
        req.env_vars.len(),
    );
    crate::classify_missing_workspace_spawn_failure(error, &req.project_root, message)
}
