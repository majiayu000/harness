use harness_core::error::HarnessError;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
const CONTAINER_OUTPUT_SCHEMA_DIR: &str = "/harness-output-schema";
pub(super) fn rewrite_for_container(
    args: &[OsString],
) -> Result<(Vec<OsString>, Option<PathBuf>), HarnessError> {
    let mut child_args = Vec::with_capacity(args.len());
    let mut mount = None;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        child_args.push(arg.clone());
        if arg == "--output-schema" {
            let Some(path) = iter.next() else { break };
            child_args.push(child_schema_path(&PathBuf::from(path), &mut mount)?);
        }
    }
    Ok((child_args, mount))
}
pub(super) fn mount_arg(source: &Path) -> OsString {
    OsString::from(format!(
        "type=bind,src={},dst={CONTAINER_OUTPUT_SCHEMA_DIR},readonly",
        source.display()
    ))
}
fn child_schema_path(
    host_path: &Path,
    mount: &mut Option<PathBuf>,
) -> Result<OsString, HarnessError> {
    if !host_path.is_absolute() {
        return Ok(host_path.as_os_str().to_os_string());
    }
    let (Some(parent), Some(file_name)) = (host_path.parent(), host_path.file_name()) else {
        return Err(HarnessError::AgentExecution(
            "invalid codex output schema path".to_string(),
        ));
    };
    let parent = std::fs::canonicalize(parent).map_err(|error| {
        HarnessError::AgentExecution(format!(
            "failed to resolve codex output schema directory: {error}"
        ))
    })?;
    mount.get_or_insert(parent);
    Ok(PathBuf::from(CONTAINER_OUTPUT_SCHEMA_DIR)
        .join(file_name)
        .into_os_string())
}
