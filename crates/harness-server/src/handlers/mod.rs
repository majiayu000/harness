pub mod classify;
pub mod cross_review;
pub mod dashboard;
pub mod exec;
pub mod gc;
pub mod health;
pub mod learn;
pub mod observe;
pub mod overview;
pub mod preflight;
pub mod projects;
pub mod rules;
pub mod runtime_hosts;
pub mod runtime_project_cache;
pub mod skills;
pub mod thread;
pub mod token_usage;

#[cfg(test)]
mod runtime_project_cache_api_tests;

#[cfg(test)]
mod runtime_hosts_api_tests;

/// Validate a project root path, returning early with an `INTERNAL_ERROR`
/// response on failure.
///
/// # Example
/// ```ignore
/// let project_root = validate_root!(&project_root, id, &state.core.home_dir);
/// ```
#[macro_export]
macro_rules! validate_root {
    ($path:expr, $id:expr, $home:expr) => {
        match $crate::handlers::validate_project_root($path, $home) {
            Ok(p) => p,
            Err(e) => {
                return harness_protocol::methods::RpcResponse::error(
                    $id,
                    harness_protocol::methods::INTERNAL_ERROR,
                    e,
                )
            }
        }
    };
}

/// Validate that `file` is an existing path within `project_root` (already canonicalized).
/// Returns the canonicalized file path on success.
pub(crate) fn validate_file_in_root(
    file: &std::path::Path,
    project_root: &std::path::Path,
) -> Result<std::path::PathBuf, String> {
    let canonical = file
        .canonicalize()
        .map_err(|e| format!("invalid file path '{}': {e}", file.display()))?;
    if !canonical.starts_with(project_root) {
        return Err(format!(
            "file path '{}' is outside project root '{}'",
            canonical.display(),
            project_root.display()
        ));
    }
    Ok(canonical)
}

/// Validate that a project root is an existing directory within the given home
/// directory. `home` should be captured once at server startup to avoid TOCTOU
/// races from reading `$HOME` per-request.
/// Returns the canonicalized path on success.
pub(crate) fn validate_project_root(
    path: &std::path::Path,
    home: &std::path::Path,
) -> Result<std::path::PathBuf, String> {
    let canonical = path
        .canonicalize()
        .map_err(|e| format!("invalid project root '{}': {e}", path.display()))?;
    if !canonical.is_dir() {
        return Err(format!(
            "project root is not a directory: {}",
            canonical.display()
        ));
    }
    if !canonical.starts_with(home) {
        return Err(format!(
            "project root must be within HOME: {}",
            canonical.display()
        ));
    }
    Ok(canonical)
}
