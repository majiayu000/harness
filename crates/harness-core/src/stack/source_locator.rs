use super::{
    AgentStackComponentError, AgentStackSourceLocator, AgentStackSourceScope,
    AgentStackUserGlobalRoot,
};
use std::path::{Component, Path, PathBuf};
use uuid::Uuid;

pub(super) fn validate_source_locator(
    scope: AgentStackSourceScope,
    locator: &str,
) -> Result<(), AgentStackComponentError> {
    match scope {
        AgentStackSourceScope::Repository | AgentStackSourceScope::Admin => {
            validate_portable_path(locator)?;
            reject_reserved_segments(locator)
        }
        AgentStackSourceScope::UserGlobal => validate_user_global_locator(locator),
        AgentStackSourceScope::System
        | AgentStackSourceScope::Runtime
        | AgentStackSourceScope::Runner => validate_logical_locator(scope, locator),
    }
}

fn validate_portable_path(value: &str) -> Result<(), AgentStackComponentError> {
    let drive_prefixed = value
        .as_bytes()
        .get(0..2)
        .is_some_and(|head| head[0].is_ascii_alphabetic() && head[1] == b':');
    if value.is_empty()
        || value.starts_with('/')
        || value.contains('\\')
        || value.contains('\0')
        || drive_prefixed
        || value
            .split('/')
            .any(|segment| segment.is_empty() || segment == "." || segment == "..")
    {
        Err(AgentStackComponentError::InvalidSourceLocator)
    } else {
        Ok(())
    }
}

fn validate_user_global_locator(locator: &str) -> Result<(), AgentStackComponentError> {
    validate_portable_path(locator)?;
    let mut segments = locator.split('/');
    let Some(root) = segments.next() else {
        return Err(AgentStackComponentError::InvalidSourceLocator);
    };
    match root {
        "home_harness" | "xdg_config_harness" | "platform_config_harness" => {}
        "configured_user" => validate_configured_user_key(
            segments
                .next()
                .ok_or(AgentStackComponentError::InvalidSourceLocator)?,
        )?,
        _ => return Err(AgentStackComponentError::InvalidSourceLocator),
    }
    if segments.next().is_none() {
        return Err(AgentStackComponentError::InvalidSourceLocator);
    }
    reject_reserved_segments(locator)
}

fn validate_logical_locator(
    scope: AgentStackSourceScope,
    locator: &str,
) -> Result<(), AgentStackComponentError> {
    validate_portable_path(locator)?;
    let mut segments = locator.split('/');
    if scope == AgentStackSourceScope::System && segments.next() != Some("builtin") {
        return Err(AgentStackComponentError::InvalidSourceLocator);
    }
    let namespace = segments
        .next()
        .ok_or(AgentStackComponentError::InvalidSourceLocator)?;
    let remaining = segments.collect::<Vec<_>>();
    if !is_snake_case(namespace)
        || is_uuid_shaped(namespace)
        || remaining.is_empty()
        || remaining
            .iter()
            .any(|segment| !valid_logical_segment(segment))
    {
        return Err(AgentStackComponentError::InvalidSourceLocator);
    }
    reject_reserved_segments(locator)
}

pub(super) fn valid_logical_segments(value: &str) -> bool {
    !value.is_empty() && value.split('/').all(valid_logical_segment)
}

fn valid_logical_segment(segment: &str) -> bool {
    segment
        .as_bytes()
        .first()
        .is_some_and(u8::is_ascii_alphanumeric)
        && segment
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
        && !is_reserved(segment)
        && !is_uuid_shaped(segment)
}

fn validate_configured_user_key(key: &str) -> Result<(), AgentStackComponentError> {
    if is_snake_case(key) && !is_reserved(key) && !is_uuid_shaped(key) {
        Ok(())
    } else {
        Err(AgentStackComponentError::InvalidSourceLocator)
    }
}

pub(super) fn is_snake_case(value: &str) -> bool {
    !value.is_empty()
        && !value.starts_with('_')
        && !value.ends_with('_')
        && !value.contains("__")
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
}

fn reject_reserved_segments(value: &str) -> Result<(), AgentStackComponentError> {
    if value.split('/').any(is_reserved) {
        Err(AgentStackComponentError::InvalidSourceLocator)
    } else {
        Ok(())
    }
}

#[rustfmt::skip]
pub(super) fn is_reserved(value: &str) -> bool {
    ["unknown", "unknown-component", "unknown_component", "missing", "null", "none"]
        .iter()
        .any(|reserved| value.eq_ignore_ascii_case(reserved))
}

pub(super) fn is_uuid_shaped(value: &str) -> bool {
    Uuid::parse_str(value).is_ok()
}

pub fn resolve_xdg_config_harness_root(
    xdg_config_home: Option<&Path>,
    home: Option<&Path>,
) -> Result<PathBuf, AgentStackComponentError> {
    let root = if let Some(xdg) = xdg_config_home.filter(|path| path.is_absolute()) {
        xdg.join("harness")
    } else if let Some(home) = home.filter(|path| path.is_absolute()) {
        home.join(".config").join("harness")
    } else {
        return Err(AgentStackComponentError::XdgConfigRootUnavailable);
    };
    normalize_absolute_path(&root)
}

pub fn select_user_global_root(
    source: &Path,
    home_harness: Option<&Path>,
    xdg_config_harness: Option<&Path>,
    platform_config_harness: Option<&Path>,
    configured_user_roots: &[(&str, &Path)],
) -> Result<(AgentStackUserGlobalRoot, AgentStackSourceLocator), AgentStackComponentError> {
    if configured_user_roots.len() > 1 {
        return Err(AgentStackComponentError::AmbiguousConfiguredUserRoot);
    }
    if let Some((key, _)) = configured_user_roots.first() {
        validate_configured_user_key(key)?;
    }
    for (root_kind, namespace, root) in [
        (
            AgentStackUserGlobalRoot::HomeHarness,
            "home_harness",
            home_harness,
        ),
        (
            AgentStackUserGlobalRoot::XdgConfigHarness,
            "xdg_config_harness",
            xdg_config_harness,
        ),
        (
            AgentStackUserGlobalRoot::PlatformConfigHarness,
            "platform_config_harness",
            platform_config_harness,
        ),
    ] {
        if let Some(root) = root {
            if let Some(relative) = relative_portable_path_if_within(root, source)? {
                return user_root_result(root_kind, format!("{namespace}/{relative}"));
            }
        }
    }
    if let Some((key, root)) = configured_user_roots.first() {
        if let Some(relative) = relative_portable_path_if_within(root, source)? {
            return user_root_result(
                AgentStackUserGlobalRoot::ConfiguredUser,
                format!("configured_user/{key}/{relative}"),
            );
        }
    }
    Err(AgentStackComponentError::UntypedDiscoverySource)
}

fn user_root_result(
    root: AgentStackUserGlobalRoot,
    locator: String,
) -> Result<(AgentStackUserGlobalRoot, AgentStackSourceLocator), AgentStackComponentError> {
    validate_user_global_locator(&locator)?;
    Ok((root, AgentStackSourceLocator(locator)))
}

pub(super) fn relative_portable_path(
    root: &Path,
    source: &Path,
) -> Result<String, AgentStackComponentError> {
    relative_portable_path_if_within(root, source)?
        .ok_or(AgentStackComponentError::SourceOutsideRoot)
}

fn relative_portable_path_if_within(
    root: &Path,
    source: &Path,
) -> Result<Option<String>, AgentStackComponentError> {
    let root = absolute_path_key(root, true)?;
    let source = absolute_path_key(source, false)?;
    if !root_key_matches(root.drive, &root.segments, source.drive, &source.segments)
        || source.segments.len() <= root.segments.len()
    {
        return Ok(None);
    }
    let relative = source.segments[root.segments.len()..].join("/");
    validate_portable_path(&relative)?;
    Ok(Some(relative))
}

fn normalize_absolute_path(path: &Path) -> Result<PathBuf, AgentStackComponentError> {
    let key = absolute_path_key(path, true)?;
    let mut result = PathBuf::new();
    #[cfg(unix)]
    result.push("/");
    #[cfg(windows)]
    if let Some(drive) = key.drive {
        result.push(format!("{}:\\", drive as char));
    }
    for segment in key.segments {
        result.push(segment);
    }
    Ok(result)
}

#[derive(Debug, PartialEq, Eq)]
struct AbsolutePathKey {
    drive: Option<u8>,
    segments: Vec<String>,
}

fn absolute_path_key(
    path: &Path,
    resolve_parent: bool,
) -> Result<AbsolutePathKey, AgentStackComponentError> {
    if !path.is_absolute() {
        return Err(AgentStackComponentError::InvalidSourceLocator);
    }
    #[cfg(windows)]
    let mut drive = None;
    #[cfg(not(windows))]
    let drive = None;
    let mut segments = Vec::new();
    for component in path.components() {
        match component {
            Component::Prefix(value) => {
                #[cfg(windows)]
                {
                    use std::path::Prefix;
                    drive = Some(match value.kind() {
                        Prefix::Disk(drive) | Prefix::VerbatimDisk(drive) => {
                            drive.to_ascii_uppercase()
                        }
                        _ => return Err(AgentStackComponentError::InvalidSourceLocator),
                    });
                }
                #[cfg(not(windows))]
                {
                    let _ = value;
                    return Err(AgentStackComponentError::InvalidSourceLocator);
                }
            }
            Component::RootDir | Component::CurDir => {}
            Component::ParentDir if resolve_parent && !segments.is_empty() => {
                segments.pop();
            }
            Component::ParentDir => return Err(AgentStackComponentError::InvalidSourceLocator),
            Component::Normal(value) => segments.push(
                value
                    .to_str()
                    .ok_or(AgentStackComponentError::NonUtf8SourceLocator)?
                    .to_owned(),
            ),
        }
    }
    Ok(AbsolutePathKey { drive, segments })
}

fn drives_match(left: Option<u8>, right: Option<u8>) -> bool {
    left.map(|drive| drive.to_ascii_uppercase()) == right.map(|drive| drive.to_ascii_uppercase())
}

#[rustfmt::skip]
fn root_key_matches<T: PartialEq>(
    root_drive: Option<u8>, root: &[T], source_drive: Option<u8>, source: &[T],
) -> bool {
    drives_match(root_drive, source_drive) && source.starts_with(root)
}

#[cfg(test)]
#[rustfmt::skip]
pub(super) fn root_keys_match_for_test(
    drive_a: Option<u8>, segments_a: &[&str], drive_b: Option<u8>, segments_b: &[&str],
) -> bool {
    segments_a.len() == segments_b.len()
        && root_key_matches(drive_a, segments_a, drive_b, segments_b)
}
