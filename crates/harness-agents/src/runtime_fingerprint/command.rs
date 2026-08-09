//! Native command evidence and lazy Unix candidate derivation.

use super::RuntimeFingerprintProduceError;
use harness_core::stack::Sha256Digest;
use std::ffi::OsStr;

const COMMAND_DIGEST_DOMAIN: &[u8] = b"harness_runtime_configured_command_v0_1\0";
const WORKING_DIRECTORY_DIGEST_DOMAIN: &[u8] = b"harness_runtime_working_directory_v0_1\0";
#[cfg(unix)]
const CANDIDATE_DIGEST_DOMAIN: &[u8] = b"harness_runtime_resolution_candidate_v0_1\0";

#[cfg(unix)]
use harness_core::stack::fingerprint::RuntimeCommandForm;
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;
#[cfg(any(
    test,
    all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    )
))]
use std::os::unix::ffi::OsStringExt;
#[cfg(unix)]
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg(unix)]
pub(super) enum CandidateReference {
    Absolute(PathBuf),
    WorkingDirectoryRelative(PathBuf),
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg(unix)]
pub(super) struct ResolvedCandidate {
    pub(super) reference: CandidateReference,
    pub(super) candidate_digest: Sha256Digest,
}

#[derive(Debug, Clone)]
#[cfg(unix)]
enum CandidatePlan {
    Unusable,
    Single(ResolvedCandidate),
    Bare {
        command: Vec<u8>,
        working_directory: Vec<u8>,
        path: Vec<u8>,
    },
}

#[derive(Debug, Clone)]
#[cfg(unix)]
pub(super) struct PreparedCommand {
    pub(super) command_form: RuntimeCommandForm,
    pub(super) configured_command_digest: Sha256Digest,
    pub(super) working_directory_digest: Sha256Digest,
    plan: CandidatePlan,
    #[cfg(test)]
    derivation_count: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

#[cfg(unix)]
impl PreparedCommand {
    pub(super) fn validate_shape(&self) -> bool {
        let plan_valid = match (&self.command_form, &self.plan) {
            (RuntimeCommandForm::UnixBare, CandidatePlan::Unusable) => true,
            (
                RuntimeCommandForm::UnixBare,
                CandidatePlan::Bare {
                    command,
                    working_directory,
                    path,
                },
            ) => {
                !command.is_empty()
                    && !working_directory.is_empty()
                    && !command.contains(&0)
                    && !working_directory.contains(&0)
                    && !path.contains(&0)
            }
            (
                RuntimeCommandForm::UnixAbsolute | RuntimeCommandForm::UnixQualified,
                CandidatePlan::Single(candidate),
            ) => candidate_path(candidate).is_some_and(|path| !path.as_os_str().is_empty()),
            _ => false,
        };
        plan_valid
            && !self.configured_command_digest.as_str().is_empty()
            && !self.working_directory_digest.as_str().is_empty()
    }

    #[cfg(any(
        test,
        all(
            target_os = "linux",
            any(target_arch = "x86_64", target_arch = "aarch64")
        )
    ))]
    pub(super) fn path_unusable(&self) -> bool {
        matches!(self.plan, CandidatePlan::Unusable)
    }

    #[cfg(all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ))]
    pub(super) fn candidate_capacity(&self) -> usize {
        match self.plan {
            CandidatePlan::Unusable => 0,
            CandidatePlan::Single(_) => 1,
            CandidatePlan::Bare { .. } => super::RUNTIME_FINGERPRINT_MAX_RESOLUTION_CANDIDATES,
        }
    }

    #[cfg(any(
        test,
        all(
            target_os = "linux",
            any(target_arch = "x86_64", target_arch = "aarch64")
        )
    ))]
    pub(super) fn has_candidate(&self, index: usize) -> bool {
        match &self.plan {
            CandidatePlan::Unusable => false,
            CandidatePlan::Single(_) => index == 0,
            CandidatePlan::Bare { path, .. } => {
                path.split(|byte| *byte == b':').nth(index).is_some()
            }
        }
    }

    #[cfg(any(
        test,
        all(
            target_os = "linux",
            any(target_arch = "x86_64", target_arch = "aarch64")
        )
    ))]
    pub(super) fn candidate(
        &self,
        index: usize,
    ) -> Result<Option<ResolvedCandidate>, RuntimeFingerprintProduceError> {
        let candidate = match &self.plan {
            CandidatePlan::Unusable => Ok(None),
            CandidatePlan::Single(candidate) => Ok((index == 0).then(|| candidate.clone())),
            CandidatePlan::Bare {
                command,
                working_directory,
                path,
            } => path
                .split(|byte| *byte == b':')
                .nth(index)
                .map(|entry| bare_candidate(command, working_directory, entry))
                .transpose(),
        };
        #[cfg(test)]
        if matches!(candidate, Ok(Some(_))) {
            self.derivation_count
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        candidate
    }

    #[cfg(all(
        test,
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    ))]
    pub(super) fn derivation_count(&self) -> std::sync::Arc<std::sync::atomic::AtomicUsize> {
        std::sync::Arc::clone(&self.derivation_count)
    }
}

#[cfg(unix)]
pub(super) fn prepare_command(
    command: &OsStr,
    working_directory: &Path,
    child_path: Option<&OsStr>,
) -> Result<PreparedCommand, RuntimeFingerprintProduceError> {
    let command_bytes = command.as_bytes();
    let cwd_bytes = working_directory.as_os_str().as_bytes();
    if command_bytes.is_empty()
        || command_bytes.contains(&0)
        || cwd_bytes.is_empty()
        || cwd_bytes.contains(&0)
    {
        return Err(RuntimeFingerprintProduceError::InvalidLaunchContext);
    }
    let command_form = if command_bytes.first() == Some(&b'/') {
        RuntimeCommandForm::UnixAbsolute
    } else if command_bytes.contains(&b'/') {
        RuntimeCommandForm::UnixQualified
    } else {
        RuntimeCommandForm::UnixBare
    };
    let plan = match command_form {
        RuntimeCommandForm::UnixAbsolute => CandidatePlan::Single(resolved_candidate(
            CandidateReference::Absolute(PathBuf::from(command)),
            command_bytes,
        )),
        RuntimeCommandForm::UnixQualified => CandidatePlan::Single(resolved_candidate(
            CandidateReference::WorkingDirectoryRelative(PathBuf::from(command)),
            &lexical_join(cwd_bytes, command_bytes)?,
        )),
        RuntimeCommandForm::UnixBare => match child_path {
            None => CandidatePlan::Unusable,
            Some(path) if path.as_bytes().contains(&0) => {
                return Err(RuntimeFingerprintProduceError::InvalidLaunchContext)
            }
            Some(path) => CandidatePlan::Bare {
                command: command_bytes.to_vec(),
                working_directory: cwd_bytes.to_vec(),
                path: path.as_bytes().to_vec(),
            },
        },
        _ => return Err(RuntimeFingerprintProduceError::InvalidLaunchContext),
    };
    Ok(PreparedCommand {
        command_form,
        configured_command_digest: digest_native_os_string(COMMAND_DIGEST_DOMAIN, command),
        working_directory_digest: digest_native_os_string(
            WORKING_DIRECTORY_DIGEST_DOMAIN,
            working_directory.as_os_str(),
        ),
        plan,
        #[cfg(test)]
        derivation_count: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
    })
}

#[cfg(any(
    test,
    all(
        target_os = "linux",
        any(target_arch = "x86_64", target_arch = "aarch64")
    )
))]
fn bare_candidate(
    command: &[u8],
    working_directory: &[u8],
    entry: &[u8],
) -> Result<ResolvedCandidate, RuntimeFingerprintProduceError> {
    let relative = entry.is_empty() || entry.first() != Some(&b'/');
    let reference_bytes = if entry.is_empty() {
        command.to_vec()
    } else {
        lexical_join(entry, command)?
    };
    let lexical = if relative {
        lexical_join(working_directory, &reference_bytes)?
    } else {
        reference_bytes.clone()
    };
    let path = PathBuf::from(std::ffi::OsString::from_vec(reference_bytes));
    let reference = if relative {
        CandidateReference::WorkingDirectoryRelative(path)
    } else {
        CandidateReference::Absolute(path)
    };
    Ok(resolved_candidate(reference, &lexical))
}

#[cfg(unix)]
fn candidate_path(candidate: &ResolvedCandidate) -> Option<&Path> {
    match &candidate.reference {
        CandidateReference::Absolute(path) | CandidateReference::WorkingDirectoryRelative(path) => {
            Some(path)
        }
    }
}

#[cfg(unix)]
fn resolved_candidate(reference: CandidateReference, lexical: &[u8]) -> ResolvedCandidate {
    ResolvedCandidate {
        reference,
        candidate_digest: digest_unix_bytes(CANDIDATE_DIGEST_DOMAIN, lexical),
    }
}

#[cfg(unix)]
fn lexical_join(left: &[u8], right: &[u8]) -> Result<Vec<u8>, RuntimeFingerprintProduceError> {
    let needs_separator = !left.is_empty() && left.last() != Some(&b'/');
    let capacity = left
        .len()
        .checked_add(usize::from(needs_separator))
        .and_then(|value| value.checked_add(right.len()))
        .filter(|value| *value <= 3 * super::RUNTIME_FINGERPRINT_MAX_LAUNCH_INPUT_UNITS + 2)
        .ok_or(RuntimeFingerprintProduceError::InvalidLaunchContext)?;
    let mut value = Vec::with_capacity(capacity);
    value.extend_from_slice(left);
    if needs_separator {
        value.push(b'/');
    }
    value.extend_from_slice(right);
    Ok(value)
}

pub(super) fn digest_native_os_string(domain: &[u8], value: &OsStr) -> Sha256Digest {
    let mut framed = Vec::with_capacity(domain.len() + value.len() + 16);
    framed.extend_from_slice(domain);
    append_native_os_string(&mut framed, value);
    Sha256Digest::from_bytes(&framed)
}

#[cfg(unix)]
fn digest_unix_bytes(domain: &[u8], value: &[u8]) -> Sha256Digest {
    let mut framed = Vec::with_capacity(domain.len() + value.len() + 13);
    framed.extend_from_slice(domain);
    framed.extend_from_slice(b"unix\0");
    framed.extend_from_slice(&(value.len() as u64).to_be_bytes());
    framed.extend_from_slice(value);
    Sha256Digest::from_bytes(&framed)
}

#[cfg(unix)]
fn append_native_os_string(output: &mut Vec<u8>, value: &OsStr) {
    let bytes = value.as_bytes();
    output.extend_from_slice(b"unix\0");
    output.extend_from_slice(&(bytes.len() as u64).to_be_bytes());
    output.extend_from_slice(bytes);
}

#[cfg(windows)]
fn append_native_os_string(output: &mut Vec<u8>, value: &OsStr) {
    use std::os::windows::ffi::OsStrExt;

    let units = value.encode_wide().collect::<Vec<_>>();
    output.extend_from_slice(b"windows\0");
    output.extend_from_slice(&(units.len() as u64).to_be_bytes());
    for unit in units {
        output.extend_from_slice(&unit.to_le_bytes());
    }
}
