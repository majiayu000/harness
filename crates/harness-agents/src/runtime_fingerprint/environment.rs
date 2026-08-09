//! Closed runtime environment policy.

use super::executable::{digest_native_os_string, native_os_units_len};
use super::{
    RuntimeFingerprintProduceError, RuntimeLaunchInputLimitKind,
    RUNTIME_FINGERPRINT_MAX_ENVIRONMENT_KEY_UNITS, RUNTIME_FINGERPRINT_MAX_LAUNCH_INPUT_UNITS,
    RUNTIME_FINGERPRINT_MAX_OBSERVATION_ENV_ENTRIES, RUNTIME_FINGERPRINT_MAX_SETUP_SECRET_NAMES,
    RUNTIME_FINGERPRINT_MAX_SETUP_SECRET_NAME_UNITS,
};
use harness_core::stack::fingerprint::{
    LocalExecutableRuntimeKind, RuntimeEnvironmentFact, RuntimeEnvironmentKey,
    RuntimeEnvironmentValue, RuntimeProbeFailure, RuntimeProbeFailureDetail,
    RuntimeProbeFailureKind, RuntimeVersionFacts, RuntimeVersionStream,
};
use harness_core::stack::Sha256Digest;
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};

const PATH_DIGEST_DOMAIN: &[u8] = b"harness_runtime_environment_path_v0_1\0";
const CLAUDE_CONFIG_DIR_DIGEST_DOMAIN: &[u8] =
    b"harness_runtime_environment_claude_config_dir_v0_1\0";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeTermination {
    Exit(i32),
    Signal,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(untagged)]
pub enum RuntimeOutputClassification {
    Version(RuntimeVersionFacts),
    Failure(RuntimeProbeFailure),
}

pub fn classify_completed_runtime_output(
    kind: LocalExecutableRuntimeKind,
    stdout: &[u8],
    stderr: &[u8],
    termination: RuntimeTermination,
) -> Result<RuntimeOutputClassification, RuntimeFingerprintProduceError> {
    match termination {
        RuntimeTermination::Signal => {
            return output_failure(RuntimeProbeFailureKind::TerminatedBySignal, None);
        }
        RuntimeTermination::Exit(code) if code != 0 => {
            return output_failure(
                RuntimeProbeFailureKind::NonzeroExit,
                Some(RuntimeProbeFailureDetail::ExitCode(code)),
            );
        }
        RuntimeTermination::Exit(_) => {}
    }
    let stdout_text = match std::str::from_utf8(stdout) {
        Ok(value) => value,
        Err(_) => return output_failure(RuntimeProbeFailureKind::InvalidUtf8, None),
    };
    let stderr_text = match std::str::from_utf8(stderr) {
        Ok(value) => value,
        Err(_) => return output_failure(RuntimeProbeFailureKind::InvalidUtf8, None),
    };
    let stdout_match = parse_product_output(kind, stdout_text);
    let stderr_match = parse_product_output(kind, stderr_text);
    let stdout_blank = ascii_blank(stdout);
    let stderr_blank = ascii_blank(stderr);
    match (stdout_match, stderr_match, stdout_blank, stderr_blank) {
        (Some(version), None, _, true) => {
            output_version(version, stdout, stderr, RuntimeVersionStream::Stdout)
        }
        (None, Some(version), true, _) => {
            output_version(version, stdout, stderr, RuntimeVersionStream::Stderr)
        }
        (Some(_), Some(_), _, _) => output_failure(RuntimeProbeFailureKind::AmbiguousVersion, None),
        (None, None, true, true) => output_failure(RuntimeProbeFailureKind::EmptyOutput, None),
        _ => output_failure(RuntimeProbeFailureKind::UnparseableVersion, None),
    }
}

fn output_version(
    normalized_version: String,
    stdout: &[u8],
    stderr: &[u8],
    selected_stream: RuntimeVersionStream,
) -> Result<RuntimeOutputClassification, RuntimeFingerprintProduceError> {
    Ok(RuntimeOutputClassification::Version(
        RuntimeVersionFacts::new(
            normalized_version,
            Sha256Digest::from_bytes(stdout),
            Sha256Digest::from_bytes(stderr),
            selected_stream,
        )?,
    ))
}

fn output_failure(
    kind: RuntimeProbeFailureKind,
    detail: Option<RuntimeProbeFailureDetail>,
) -> Result<RuntimeOutputClassification, RuntimeFingerprintProduceError> {
    let failure = match detail {
        Some(detail) => RuntimeProbeFailure::with_detail(kind, detail)?,
        None => RuntimeProbeFailure::new(kind)?,
    };
    Ok(RuntimeOutputClassification::Failure(failure))
}

fn parse_product_output(kind: LocalExecutableRuntimeKind, value: &str) -> Option<String> {
    let line = strip_optional_line_ending(value)?;
    let version = match kind {
        LocalExecutableRuntimeKind::CodexExec | LocalExecutableRuntimeKind::CodexJsonrpc => {
            line.strip_prefix("codex-cli ")?
        }
        LocalExecutableRuntimeKind::ClaudeCode => line.strip_suffix(" (Claude Code)")?,
    };
    semver_is_valid(version).then(|| version.to_owned())
}

fn strip_optional_line_ending(value: &str) -> Option<&str> {
    let stripped = if let Some(value) = value.strip_suffix("\r\n") {
        value
    } else if let Some(value) = value.strip_suffix('\n') {
        value
    } else {
        value
    };
    (!stripped.contains(['\r', '\n'])).then_some(stripped)
}

fn ascii_blank(value: &[u8]) -> bool {
    value
        .iter()
        .all(|byte| matches!(byte, b'\t' | b'\n' | b'\r' | b' '))
}

fn semver_is_valid(value: &str) -> bool {
    if value.is_empty() || !value.is_ascii() {
        return false;
    }
    let (without_build, build) = match value.split_once('+') {
        Some((left, right)) if !right.contains('+') => (left, Some(right)),
        Some(_) => return false,
        None => (value, None),
    };
    if let Some(build) = build {
        if !valid_identifiers(build, false) {
            return false;
        }
    }
    let (core, prerelease) = match without_build.split_once('-') {
        Some((core, prerelease)) => (core, Some(prerelease)),
        None => (without_build, None),
    };
    if let Some(prerelease) = prerelease {
        if !valid_identifiers(prerelease, true) {
            return false;
        }
    }
    let mut components = core.split('.');
    let valid = (0..3).all(|_| components.next().is_some_and(valid_numeric_component));
    valid && components.next().is_none()
}

fn valid_numeric_component(value: &str) -> bool {
    !value.is_empty()
        && value.bytes().all(|byte| byte.is_ascii_digit())
        && (value == "0" || !value.starts_with('0'))
}

fn valid_identifiers(value: &str, reject_numeric_leading_zero: bool) -> bool {
    !value.is_empty()
        && value.split('.').all(|identifier| {
            !identifier.is_empty()
                && identifier
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
                && (!reject_numeric_leading_zero
                    || !identifier.bytes().all(|byte| byte.is_ascii_digit())
                    || valid_numeric_component(identifier))
        })
}

#[derive(Debug, Clone)]
pub(super) struct SelectedEnvironment {
    pub(super) facts: Vec<RuntimeEnvironmentFact>,
    pub(super) child_path: Option<OsString>,
}

pub(super) fn validate_and_select(
    kind: LocalExecutableRuntimeKind,
    entries: &[(OsString, OsString)],
    setup_secret_names: &[OsString],
) -> Result<SelectedEnvironment, RuntimeFingerprintProduceError> {
    validate_collection_limits(entries, setup_secret_names)?;

    let mut canonical_entries = BTreeMap::<String, &OsString>::new();
    for (key, value) in entries {
        let canonical = canonical_environment_key(key)?;
        if canonical_entries.insert(canonical, value).is_some() {
            return Err(RuntimeFingerprintProduceError::EnvironmentKeyCollision);
        }
    }

    let mut exclusions = BTreeSet::new();
    for name in setup_secret_names {
        let canonical = canonical_environment_key(name)?;
        if !exclusions.insert(canonical) {
            return Err(RuntimeFingerprintProduceError::EnvironmentKeyCollision);
        }
    }

    let selected_path = selected_value("PATH", &canonical_entries, &exclusions);
    if let Some(value) = selected_path {
        ensure_value_limit(value, RuntimeLaunchInputLimitKind::ChildPath)?;
    }
    let selected_claude = selected_value("CLAUDE_CONFIG_DIR", &canonical_entries, &exclusions);
    if matches!(kind, LocalExecutableRuntimeKind::ClaudeCode) {
        if let Some(value) = selected_claude {
            ensure_value_limit(value, RuntimeLaunchInputLimitKind::ClaudeConfigDirectory)?;
        }
    }

    let facts = match kind {
        LocalExecutableRuntimeKind::CodexExec | LocalExecutableRuntimeKind::CodexJsonrpc => vec![
            secret_fact(
                RuntimeEnvironmentKey::OpenaiApiKey,
                "OPENAI_API_KEY",
                &canonical_entries,
                &exclusions,
            ),
            digest_fact(
                RuntimeEnvironmentKey::Path,
                selected_path,
                PATH_DIGEST_DOMAIN,
            ),
        ],
        LocalExecutableRuntimeKind::ClaudeCode => vec![
            secret_fact(
                RuntimeEnvironmentKey::AnthropicApiKey,
                "ANTHROPIC_API_KEY",
                &canonical_entries,
                &exclusions,
            ),
            digest_fact(
                RuntimeEnvironmentKey::ClaudeConfigDir,
                selected_claude,
                CLAUDE_CONFIG_DIR_DIGEST_DOMAIN,
            ),
            digest_fact(
                RuntimeEnvironmentKey::Path,
                selected_path,
                PATH_DIGEST_DOMAIN,
            ),
        ],
    };

    Ok(SelectedEnvironment {
        facts,
        child_path: selected_path.cloned(),
    })
}

fn validate_collection_limits(
    entries: &[(OsString, OsString)],
    setup_secret_names: &[OsString],
) -> Result<(), RuntimeFingerprintProduceError> {
    if entries.len() > RUNTIME_FINGERPRINT_MAX_OBSERVATION_ENV_ENTRIES {
        return Err(RuntimeFingerprintProduceError::LaunchInputLimitExceeded(
            RuntimeLaunchInputLimitKind::ObservationEnvironmentEntries,
        ));
    }
    for (key, _) in entries {
        if native_os_units_len(key, RUNTIME_FINGERPRINT_MAX_ENVIRONMENT_KEY_UNITS)
            > RUNTIME_FINGERPRINT_MAX_ENVIRONMENT_KEY_UNITS
        {
            return Err(RuntimeFingerprintProduceError::LaunchInputLimitExceeded(
                RuntimeLaunchInputLimitKind::EnvironmentKey,
            ));
        }
    }
    if setup_secret_names.len() > RUNTIME_FINGERPRINT_MAX_SETUP_SECRET_NAMES {
        return Err(RuntimeFingerprintProduceError::LaunchInputLimitExceeded(
            RuntimeLaunchInputLimitKind::SetupSecretNames,
        ));
    }
    for name in setup_secret_names {
        if native_os_units_len(name, RUNTIME_FINGERPRINT_MAX_SETUP_SECRET_NAME_UNITS)
            > RUNTIME_FINGERPRINT_MAX_SETUP_SECRET_NAME_UNITS
        {
            return Err(RuntimeFingerprintProduceError::LaunchInputLimitExceeded(
                RuntimeLaunchInputLimitKind::SetupSecretName,
            ));
        }
    }
    Ok(())
}

fn ensure_value_limit(
    value: &OsStr,
    kind: RuntimeLaunchInputLimitKind,
) -> Result<(), RuntimeFingerprintProduceError> {
    if native_os_units_len(value, RUNTIME_FINGERPRINT_MAX_LAUNCH_INPUT_UNITS)
        > RUNTIME_FINGERPRINT_MAX_LAUNCH_INPUT_UNITS
    {
        Err(RuntimeFingerprintProduceError::LaunchInputLimitExceeded(
            kind,
        ))
    } else {
        Ok(())
    }
}

fn selected_value<'a>(
    key: &str,
    entries: &'a BTreeMap<String, &'a OsString>,
    exclusions: &BTreeSet<String>,
) -> Option<&'a OsString> {
    (!exclusions.contains(key))
        .then(|| entries.get(key).copied())
        .flatten()
}

fn secret_fact(
    evidence_key: RuntimeEnvironmentKey,
    key: &str,
    entries: &BTreeMap<String, &OsString>,
    exclusions: &BTreeSet<String>,
) -> RuntimeEnvironmentFact {
    let value = if selected_value(key, entries, exclusions).is_some() {
        RuntimeEnvironmentValue::Redacted
    } else {
        RuntimeEnvironmentValue::Unset
    };
    RuntimeEnvironmentFact::new(evidence_key, value)
}

fn digest_fact(
    key: RuntimeEnvironmentKey,
    value: Option<&OsString>,
    domain: &[u8],
) -> RuntimeEnvironmentFact {
    let value = value.map_or(RuntimeEnvironmentValue::Unset, |value| {
        RuntimeEnvironmentValue::SetDigest {
            value_sha256: digest_native_os_string(domain, value),
        }
    });
    RuntimeEnvironmentFact::new(key, value)
}

#[cfg(unix)]
fn canonical_environment_key(key: &OsStr) -> Result<String, RuntimeFingerprintProduceError> {
    use std::os::unix::ffi::OsStrExt;

    let bytes = key.as_bytes();
    if bytes.is_empty() || bytes.contains(&0) || bytes.contains(&b'=') {
        return Err(RuntimeFingerprintProduceError::InvalidEnvironmentKey);
    }
    std::str::from_utf8(bytes)
        .map(str::to_owned)
        .map_err(|_| RuntimeFingerprintProduceError::InvalidEnvironmentKey)
}

#[cfg(windows)]
fn canonical_environment_key(key: &OsStr) -> Result<String, RuntimeFingerprintProduceError> {
    use std::os::windows::ffi::OsStrExt;

    let mut canonical = String::new();
    for unit in key.encode_wide() {
        if unit == 0 || unit == u16::from(b'=') || unit > 0x7f {
            return Err(RuntimeFingerprintProduceError::InvalidEnvironmentKey);
        }
        canonical.push(char::from((unit as u8).to_ascii_uppercase()));
    }
    if canonical.is_empty() {
        Err(RuntimeFingerprintProduceError::InvalidEnvironmentKey)
    } else {
        Ok(canonical)
    }
}
