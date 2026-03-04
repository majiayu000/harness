pub mod cross_review;
pub mod preflight;

use std::path::{Path, PathBuf};

/// Validate and canonicalize a project root path.
///
/// Ensures the path exists and is an absolute, canonical filesystem path.
/// Returns an error message if validation fails.
pub fn validate_project_root(path: &Path) -> Result<PathBuf, String> {
    let canonical = path
        .canonicalize()
        .map_err(|e| format!("invalid project_root {:?}: {e}", path))?;
    if !canonical.is_dir() {
        return Err(format!("project_root {:?} is not a directory", canonical));
    }
    Ok(canonical)
}
