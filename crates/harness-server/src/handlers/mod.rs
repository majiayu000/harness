pub mod exec;
pub mod gc;
pub mod health;
pub mod learn;
pub mod observe;
pub mod rules;
pub mod skills;
pub mod thread;

/// Strip non-whitespace control characters from user-supplied strings to reduce
/// prompt injection surface. Newlines and tabs are preserved for multiline inputs.
pub(crate) fn sanitize_user_input(s: &str) -> String {
    s.chars()
        .filter(|c| !c.is_control() || matches!(c, '\n' | '\r' | '\t'))
        .collect()
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
