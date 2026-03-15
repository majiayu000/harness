pub mod classify;
pub mod cross_review;
pub mod dashboard;
pub mod exec;
pub mod gc;
pub mod health;
pub mod learn;
pub mod observe;
pub mod preflight;
pub mod projects;
pub mod rules;
pub mod skills;
pub mod thread;

/// Validate a project root path, returning early with the appropriate error
/// response on failure (`INTERNAL_ERROR` for server-side misconfigurations such
/// as a missing `HOME` variable; `VALIDATION_ERROR` for bad client-supplied
/// paths).
///
/// # Example
/// ```ignore
/// let project_root = validate_root!(&project_root, id);
/// ```
#[macro_export]
macro_rules! validate_root {
    ($path:expr, $id:expr) => {
        match $crate::handlers::validate_project_root($path) {
            Ok(p) => p,
            Err((code, e)) => return harness_protocol::RpcResponse::error($id, code, e),
        }
    };
}

/// Map a [`harness_core::HarnessError`] to the most specific JSON-RPC error code.
///
/// Use this instead of hardcoding `INTERNAL_ERROR` when propagating errors from
/// the core library, so that clients receive actionable semantic codes.
pub(crate) fn harness_error_code(err: &harness_core::HarnessError) -> i32 {
    use harness_core::HarnessError::*;
    match err {
        ThreadNotFound(_) | TurnNotFound(_) | AgentNotFound(_) | DraftNotFound(_)
        | SkillNotFound(_) | ExecPlanNotFound(_) | RuleNotFound(_) => harness_protocol::NOT_FOUND,
        InvalidState(_) => harness_protocol::CONFLICT,
        Persistence(_) => harness_protocol::STORAGE_ERROR,
        AgentExecution(_) => harness_protocol::AGENT_ERROR,
        _ => harness_protocol::INTERNAL_ERROR,
    }
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
/// Returns the canonicalized path on success, or `(error_code, message)` on
/// failure.  Missing `HOME` is a server-side misconfiguration (`INTERNAL_ERROR`);
/// all other failures are client input errors (`VALIDATION_ERROR`).
pub(crate) fn validate_project_root(
    path: &std::path::Path,
) -> Result<std::path::PathBuf, (i32, String)> {
    let home = std::env::var("HOME")
        .map(std::path::PathBuf::from)
        .map_err(|_| {
            (
                harness_protocol::INTERNAL_ERROR,
                "HOME environment variable not set".to_string(),
            )
        })?;
    let canonical = path.canonicalize().map_err(|e| {
        (
            harness_protocol::VALIDATION_ERROR,
            format!("invalid project root '{}': {e}", path.display()),
        )
    })?;
    if !canonical.is_dir() {
        return Err((
            harness_protocol::VALIDATION_ERROR,
            format!("project root is not a directory: {}", canonical.display()),
        ));
    }
    if !canonical.starts_with(&home) {
        return Err((
            harness_protocol::VALIDATION_ERROR,
            format!("project root must be within HOME: {}", canonical.display()),
        ));
    }
    Ok(canonical)
}

#[cfg(test)]
mod tests {
    use super::harness_error_code;
    use harness_core::HarnessError;
    use harness_protocol::{AGENT_ERROR, CONFLICT, INTERNAL_ERROR, NOT_FOUND, STORAGE_ERROR};

    #[test]
    fn harness_error_code_maps_not_found_variants() {
        assert_eq!(
            harness_error_code(&HarnessError::ThreadNotFound("t".into())),
            NOT_FOUND
        );
        assert_eq!(
            harness_error_code(&HarnessError::TurnNotFound("t".into())),
            NOT_FOUND
        );
        assert_eq!(
            harness_error_code(&HarnessError::AgentNotFound("a".into())),
            NOT_FOUND
        );
        assert_eq!(
            harness_error_code(&HarnessError::DraftNotFound("d".into())),
            NOT_FOUND
        );
        assert_eq!(
            harness_error_code(&HarnessError::SkillNotFound("s".into())),
            NOT_FOUND
        );
        assert_eq!(
            harness_error_code(&HarnessError::ExecPlanNotFound("e".into())),
            NOT_FOUND
        );
        assert_eq!(
            harness_error_code(&HarnessError::RuleNotFound("r".into())),
            NOT_FOUND
        );
    }

    #[test]
    fn harness_error_code_maps_state_errors() {
        assert_eq!(
            harness_error_code(&HarnessError::InvalidState("bad".into())),
            CONFLICT
        );
    }

    #[test]
    fn harness_error_code_maps_persistence_errors() {
        assert_eq!(
            harness_error_code(&HarnessError::Persistence("db".into())),
            STORAGE_ERROR
        );
    }

    #[test]
    fn harness_error_code_maps_agent_errors() {
        assert_eq!(
            harness_error_code(&HarnessError::AgentExecution("fail".into())),
            AGENT_ERROR
        );
    }

    #[test]
    fn harness_error_code_falls_back_to_internal_error() {
        assert_eq!(
            harness_error_code(&HarnessError::Other("misc".into())),
            INTERNAL_ERROR
        );
    }
}
