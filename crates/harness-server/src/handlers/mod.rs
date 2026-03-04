pub mod exec;
pub mod gc;
pub mod observe;
pub mod rules;
pub mod skills;
pub mod thread;

/// Validate that a project root is an existing directory within `$HOME`.
/// Returns the canonicalized path on success.
pub(crate) fn validate_project_root(path: &std::path::Path) -> Result<std::path::PathBuf, String> {
    let home = std::env::var("HOME")
        .map(std::path::PathBuf::from)
        .map_err(|_| "HOME environment variable not set".to_string())?;
    let canonical = path
        .canonicalize()
        .map_err(|e| format!("invalid project root '{}': {e}", path.display()))?;
    if !canonical.is_dir() {
        return Err(format!(
            "project root is not a directory: {}",
            canonical.display()
        ));
    }
    if !canonical.starts_with(&home) {
        return Err(format!(
            "project root must be within HOME: {}",
            canonical.display()
        ));
    }
    Ok(canonical)
}
