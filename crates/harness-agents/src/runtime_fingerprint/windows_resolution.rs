//! Pure Windows command resolution contract for the helper-only v0.1 model.

use super::windows_candidate::WindowsResolvedCandidate;
use super::{
    RuntimeFingerprintProduceError, RuntimeLaunchInputLimitKind,
    RUNTIME_FINGERPRINT_MAX_LAUNCH_INPUT_UNITS,
};
use harness_core::stack::fingerprint::RuntimeCommandForm;
use harness_core::stack::Sha256Digest;
use std::sync::Arc;

const COMMAND_DOMAIN: &[u8] = b"harness_runtime_configured_command_v0_1\0";
pub(super) const CANDIDATE_DOMAIN: &[u8] = b"harness_runtime_resolution_candidate_v0_1\0";
const CURRENT_EXECUTABLE_DIR_DOMAIN: &[u8] =
    b"harness_runtime_windows_search_current_executable_dir_v0_1\0";
const SYSTEM_DIR_DOMAIN: &[u8] = b"harness_runtime_windows_search_system_dir_v0_1\0";
const WINDOWS_DIR_DOMAIN: &[u8] = b"harness_runtime_windows_search_windows_dir_v0_1\0";
const PARENT_PATH_DOMAIN: &[u8] = b"harness_runtime_windows_search_parent_path_v0_1\0";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowsResolutionInput {
    command: Vec<u16>,
    child_path: Option<Vec<u16>>,
    relative_base: Option<Vec<u16>>,
    current_executable_dir: Option<Vec<u16>>,
    system_dir: Option<Vec<u16>>,
    windows_dir: Option<Vec<u16>>,
    parent_path: Option<Vec<u16>>,
}

impl WindowsResolutionInput {
    pub fn new(command: Vec<u16>) -> Self {
        Self {
            command,
            child_path: None,
            relative_base: None,
            current_executable_dir: None,
            system_dir: None,
            windows_dir: None,
            parent_path: None,
        }
    }

    pub fn with_child_path(mut self, value: Option<Vec<u16>>) -> Self {
        self.child_path = value;
        self
    }
    pub fn with_relative_base(mut self, value: Option<Vec<u16>>) -> Self {
        self.relative_base = value;
        self
    }
    pub fn with_current_executable_dir(mut self, value: Option<Vec<u16>>) -> Self {
        self.current_executable_dir = value;
        self
    }
    pub fn with_system_dir(mut self, value: Option<Vec<u16>>) -> Self {
        self.system_dir = value;
        self
    }
    pub fn with_windows_dir(mut self, value: Option<Vec<u16>>) -> Self {
        self.windows_dir = value;
        self
    }
    pub fn with_parent_path(mut self, value: Option<Vec<u16>>) -> Self {
        self.parent_path = value;
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowsResolutionContextEvidence {
    current_executable_dir_digest: Option<Sha256Digest>,
    system_dir_digest: Option<Sha256Digest>,
    windows_dir_digest: Option<Sha256Digest>,
    parent_path_digest: Option<Sha256Digest>,
}

impl WindowsResolutionContextEvidence {
    pub fn current_executable_dir_digest(&self) -> Option<&Sha256Digest> {
        self.current_executable_dir_digest.as_ref()
    }
    pub fn system_dir_digest(&self) -> Option<&Sha256Digest> {
        self.system_dir_digest.as_ref()
    }
    pub fn windows_dir_digest(&self) -> Option<&Sha256Digest> {
        self.windows_dir_digest.as_ref()
    }
    pub fn parent_path_digest(&self) -> Option<&Sha256Digest> {
        self.parent_path_digest.as_ref()
    }
}

#[derive(Debug, Clone)]
pub struct WindowsResolution {
    command_form: RuntimeCommandForm,
    configured_command_digest: Sha256Digest,
    candidates: Vec<WindowsResolvedCandidate>,
    path_unusable: bool,
    context: WindowsResolutionContextEvidence,
}

impl WindowsResolution {
    pub const fn command_form(&self) -> RuntimeCommandForm {
        self.command_form
    }
    pub fn configured_command_digest(&self) -> &Sha256Digest {
        &self.configured_command_digest
    }
    pub fn candidates(&self) -> &[WindowsResolvedCandidate] {
        &self.candidates
    }
    pub const fn path_unusable(&self) -> bool {
        self.path_unusable
    }
    pub fn context(&self) -> &WindowsResolutionContextEvidence {
        &self.context
    }
}

pub fn resolve_windows_command(
    input: &WindowsResolutionInput,
) -> Result<WindowsResolution, RuntimeFingerprintProduceError> {
    validate_input(input)?;
    if input.command.is_empty() || contains_nul(&input.command) {
        return Err(RuntimeFingerprintProduceError::InvalidLaunchContext);
    }
    let command_form = classify_command_form(&input.command);
    let configured_command_digest = digest_units(COMMAND_DOMAIN, &input.command);
    let context = context_evidence(input);
    let Some(executable_command) = with_frozen_exe_extension(&input.command) else {
        return Ok(unusable(command_form, configured_command_digest, context));
    };

    let candidate_paths = match command_form {
        RuntimeCommandForm::WindowsAbsolute => {
            vec![super::windows_candidate::WindowsResolvedCandidate::exact(
                executable_command,
            )]
        }
        RuntimeCommandForm::WindowsQualified => {
            let Some(base) = input
                .relative_base
                .as_deref()
                .filter(|base| is_absolute(base))
            else {
                return Ok(unusable(command_form, configured_command_digest, context));
            };
            if input.command.contains(&u16::from(b':')) {
                return Ok(unusable(command_form, configured_command_digest, context));
            }
            let Some(candidate) = rebase_relative(base, &executable_command)? else {
                return Ok(unusable(command_form, configured_command_digest, context));
            };
            vec![super::windows_candidate::WindowsResolvedCandidate::exact(
                candidate,
            )]
        }
        RuntimeCommandForm::WindowsBare => {
            let Some(paths) = bare_search_candidates(input, &executable_command)? else {
                return Ok(unusable(command_form, configured_command_digest, context));
            };
            paths
        }
        _ => return Err(RuntimeFingerprintProduceError::InvalidLaunchContext),
    };
    Ok(WindowsResolution {
        command_form,
        configured_command_digest,
        candidates: candidate_paths,
        path_unusable: false,
        context,
    })
}

fn validate_input(input: &WindowsResolutionInput) -> Result<(), RuntimeFingerprintProduceError> {
    validate_units(
        &input.command,
        RuntimeLaunchInputLimitKind::ConfiguredCommand,
    )?;
    validate_optional(
        input.relative_base.as_deref(),
        RuntimeLaunchInputLimitKind::WorkingDirectory,
    )?;
    validate_optional(
        input.current_executable_dir.as_deref(),
        RuntimeLaunchInputLimitKind::WindowsCurrentExecutableDirectory,
    )?;
    validate_optional(
        input.system_dir.as_deref(),
        RuntimeLaunchInputLimitKind::WindowsSystemDirectory,
    )?;
    validate_optional(
        input.windows_dir.as_deref(),
        RuntimeLaunchInputLimitKind::WindowsDirectory,
    )?;
    validate_optional(
        input.parent_path.as_deref(),
        RuntimeLaunchInputLimitKind::WindowsParentPath,
    )?;
    validate_optional(
        input.child_path.as_deref(),
        RuntimeLaunchInputLimitKind::ChildPath,
    )?;
    for value in [
        input.child_path.as_deref(),
        input.relative_base.as_deref(),
        input.current_executable_dir.as_deref(),
        input.system_dir.as_deref(),
        input.windows_dir.as_deref(),
        input.parent_path.as_deref(),
    ]
    .into_iter()
    .flatten()
    {
        if contains_nul(value) {
            return Err(RuntimeFingerprintProduceError::InvalidLaunchContext);
        }
    }
    Ok(())
}

fn validate_optional(
    value: Option<&[u16]>,
    kind: RuntimeLaunchInputLimitKind,
) -> Result<(), RuntimeFingerprintProduceError> {
    value.map_or(Ok(()), |value| validate_units(value, kind))
}

fn validate_units(
    value: &[u16],
    kind: RuntimeLaunchInputLimitKind,
) -> Result<(), RuntimeFingerprintProduceError> {
    if value.len() > RUNTIME_FINGERPRINT_MAX_LAUNCH_INPUT_UNITS {
        Err(RuntimeFingerprintProduceError::LaunchInputLimitExceeded(
            kind,
        ))
    } else {
        Ok(())
    }
}

fn context_evidence(input: &WindowsResolutionInput) -> WindowsResolutionContextEvidence {
    WindowsResolutionContextEvidence {
        current_executable_dir_digest: input
            .current_executable_dir
            .as_deref()
            .map(|value| digest_units(CURRENT_EXECUTABLE_DIR_DOMAIN, value)),
        system_dir_digest: input
            .system_dir
            .as_deref()
            .map(|value| digest_units(SYSTEM_DIR_DOMAIN, value)),
        windows_dir_digest: input
            .windows_dir
            .as_deref()
            .map(|value| digest_units(WINDOWS_DIR_DOMAIN, value)),
        parent_path_digest: input
            .parent_path
            .as_deref()
            .map(|value| digest_units(PARENT_PATH_DOMAIN, value)),
    }
}

fn unusable(
    command_form: RuntimeCommandForm,
    configured_command_digest: Sha256Digest,
    context: WindowsResolutionContextEvidence,
) -> WindowsResolution {
    WindowsResolution {
        command_form,
        configured_command_digest,
        candidates: Vec::new(),
        path_unusable: true,
        context,
    }
}

fn bare_search_candidates(
    input: &WindowsResolutionInput,
    command: &[u16],
) -> Result<
    Option<Vec<super::windows_candidate::WindowsResolvedCandidate>>,
    RuntimeFingerprintProduceError,
> {
    let [Some(current_executable_dir), Some(system_dir), Some(windows_dir)] = [
        input.current_executable_dir.as_deref(),
        input.system_dir.as_deref(),
        input.windows_dir.as_deref(),
    ] else {
        return Ok(None);
    };
    let relative_base = input
        .relative_base
        .as_deref()
        .filter(|base| is_absolute(base))
        .map(Arc::<[u16]>::from);
    let command = Arc::<[u16]>::from(command.to_vec());
    let mut special_candidates = Vec::with_capacity(3);
    for directory in [current_executable_dir, system_dir, windows_dir] {
        let Some(candidate) = search_candidate(directory, relative_base.as_ref(), &command) else {
            return Ok(None);
        };
        special_candidates.push(candidate);
    }
    let mut candidates = Vec::new();
    if let Some(path) = input.child_path.as_deref() {
        if !append_path_entries(&mut candidates, path, &command, relative_base.as_ref()) {
            return Ok(None);
        }
    }
    candidates.extend(special_candidates);
    if let Some(path) = input.parent_path.as_deref() {
        if !append_path_entries(&mut candidates, path, &command, relative_base.as_ref()) {
            return Ok(None);
        }
    }
    if candidates.is_empty() {
        Ok(None)
    } else {
        Ok(Some(candidates))
    }
}

fn append_path_entries(
    candidates: &mut Vec<super::windows_candidate::WindowsResolvedCandidate>,
    path: &[u16],
    command: &Arc<[u16]>,
    relative_base: Option<&Arc<[u16]>>,
) -> bool {
    for entry in path.split(|unit| *unit == u16::from(b';')) {
        let Some(candidate) = search_candidate(entry, relative_base, command) else {
            return false;
        };
        candidates.push(candidate);
    }
    true
}

fn search_candidate(
    entry: &[u16],
    relative_base: Option<&Arc<[u16]>>,
    command: &Arc<[u16]>,
) -> Option<super::windows_candidate::WindowsResolvedCandidate> {
    if is_absolute(entry) {
        return Some(super::windows_candidate::WindowsResolvedCandidate::search(
            Arc::<[u16]>::from(entry),
            Vec::new(),
            Arc::clone(command),
        ));
    }
    let base = relative_base?;
    if entry.contains(&u16::from(b':')) {
        return None;
    }
    let (base, relative) = if is_root_relative(entry) {
        (
            Arc::<[u16]>::from(absolute_root(base)?),
            entry[1..].to_vec(),
        )
    } else {
        (Arc::clone(base), entry.to_vec())
    };
    Some(super::windows_candidate::WindowsResolvedCandidate::search(
        base,
        relative,
        Arc::clone(command),
    ))
}

fn rebase_relative(
    base: &[u16],
    value: &[u16],
) -> Result<Option<Vec<u16>>, RuntimeFingerprintProduceError> {
    if !is_root_relative(value) {
        return join(base, value).map(Some);
    }
    let Some(root) = absolute_root(base) else {
        return Ok(None);
    };
    join(&root, &value[1..]).map(Some)
}

fn is_root_relative(value: &[u16]) -> bool {
    value.first().is_some_and(|unit| is_separator(*unit))
        && !value.get(1).is_some_and(|unit| is_separator(*unit))
}

fn absolute_root(value: &[u16]) -> Option<Vec<u16>> {
    if value.len() >= 3
        && value[0] <= u16::from(u8::MAX)
        && (value[0] as u8).is_ascii_alphabetic()
        && value[1] == u16::from(b':')
        && is_separator(value[2])
    {
        return Some(vec![value[0], value[1], u16::from(b'\\')]);
    }
    let server_end = value
        .get(2..)?
        .iter()
        .position(|unit| is_separator(*unit))?
        + 2;
    let share_start = value[server_end..]
        .iter()
        .position(|unit| !is_separator(*unit))?
        + server_end;
    let share_end = value[share_start..]
        .iter()
        .position(|unit| is_separator(*unit))
        .map_or(value.len(), |offset| share_start + offset);
    (server_end > 2 && share_end > share_start).then(|| value[..share_end].to_vec())
}

fn classify_command_form(command: &[u16]) -> RuntimeCommandForm {
    if is_absolute(command) {
        RuntimeCommandForm::WindowsAbsolute
    } else if command
        .iter()
        .any(|unit| is_separator(*unit) || *unit == u16::from(b':'))
    {
        RuntimeCommandForm::WindowsQualified
    } else {
        RuntimeCommandForm::WindowsBare
    }
}

fn is_absolute(value: &[u16]) -> bool {
    let drive_absolute = value.len() >= 3
        && value[0] <= u16::from(u8::MAX)
        && (value[0] as u8).is_ascii_alphabetic()
        && value[1] == u16::from(b':')
        && is_separator(value[2]);
    let unc = value.len() >= 2 && is_separator(value[0]) && is_separator(value[1]);
    drive_absolute || unc
}

fn with_frozen_exe_extension(command: &[u16]) -> Option<Vec<u16>> {
    let basename_start = command
        .iter()
        .rposition(|unit| is_separator(*unit))
        .map_or(0, |index| index + 1);
    let extension = command[basename_start..]
        .iter()
        .rposition(|unit| *unit == u16::from(b'.'))
        .map(|index| &command[basename_start + index..]);
    match extension {
        None => {
            let mut executable = command.to_vec();
            executable.extend(".exe".encode_utf16());
            Some(executable)
        }
        Some(extension) if utf16_ascii_eq_ignore_case(extension, ".exe") => Some(command.to_vec()),
        Some(_) => None,
    }
}

fn utf16_ascii_eq_ignore_case(value: &[u16], expected: &str) -> bool {
    value.len() == expected.len()
        && value.iter().zip(expected.bytes()).all(|(left, right)| {
            *left <= u16::from(u8::MAX) && (*left as u8).eq_ignore_ascii_case(&right)
        })
}

fn join(left: &[u16], right: &[u16]) -> Result<Vec<u16>, RuntimeFingerprintProduceError> {
    let separator = usize::from(!left.last().is_some_and(|unit| is_separator(*unit)));
    let capacity = left
        .len()
        .checked_add(separator)
        .and_then(|length| length.checked_add(right.len()))
        .ok_or(RuntimeFingerprintProduceError::InvalidLaunchContext)?;
    let mut joined = Vec::with_capacity(capacity);
    joined.extend_from_slice(left);
    if separator == 1 {
        joined.push(u16::from(b'\\'));
    }
    joined.extend_from_slice(right);
    Ok(joined)
}

pub(super) fn digest_units(domain: &[u8], units: &[u16]) -> Sha256Digest {
    let mut framed = Vec::with_capacity(domain.len() + units.len() * 2 + 16);
    framed.extend_from_slice(domain);
    framed.extend_from_slice(b"windows\0");
    framed.extend_from_slice(&(units.len() as u64).to_be_bytes());
    for unit in units {
        framed.extend_from_slice(&unit.to_le_bytes());
    }
    Sha256Digest::from_bytes(&framed)
}

fn contains_nul(value: &[u16]) -> bool {
    value.contains(&0)
}

fn is_separator(unit: u16) -> bool {
    unit == u16::from(b'\\') || unit == u16::from(b'/')
}

#[cfg(test)]
#[path = "windows_resolution_contract_tests.rs"]
mod contract_tests;

#[cfg(test)]
mod tests {
    use super::*;

    fn units(value: &str) -> Vec<u16> {
        value.encode_utf16().collect()
    }

    fn paths(resolution: &WindowsResolution) -> Vec<String> {
        resolution
            .candidates()
            .iter()
            .map(|candidate| String::from_utf16(candidate.path()).unwrap())
            .collect()
    }

    #[test]
    fn context_digest_vectors_and_absent_empty_states_are_frozen() {
        let value = units("C:\\X");
        let resolution = resolve_windows_command(
            &WindowsResolutionInput::new(units("codex"))
                .with_current_executable_dir(Some(value.clone()))
                .with_system_dir(Some(value.clone()))
                .with_windows_dir(Some(value.clone()))
                .with_parent_path(Some(value)),
        )
        .unwrap();
        let context = resolution.context();
        assert_eq!(
            context.current_executable_dir_digest().unwrap().as_str(),
            "4864a078702061a4fd859437dcadfce7519d755e47de020039dd4473d3651e7e"
        );
        assert_eq!(
            context.system_dir_digest().unwrap().as_str(),
            "cc203ab9fd082171309ae3c4f28bae151cbc8d52e26870c25547977d196eb5ab"
        );
        assert_eq!(
            context.windows_dir_digest().unwrap().as_str(),
            "2fc48563c782059e0c54ca5a1c3741a991ca429434557cd070d1e87be4f7bfd7"
        );
        assert_eq!(
            context.parent_path_digest().unwrap().as_str(),
            "0206b6610a84596f5fcdda5879d0fa56bb6ab45d0c88b27b25fa9d4301327db8"
        );
        let absent = resolve_windows_command(&WindowsResolutionInput::new(units("codex"))).unwrap();
        let empty = resolve_windows_command(
            &WindowsResolutionInput::new(units("codex"))
                .with_current_executable_dir(Some(Vec::new())),
        )
        .unwrap();
        assert!(absent.context().current_executable_dir_digest().is_none());
        assert!(absent.path_unusable());
        assert!(empty.context().current_executable_dir_digest().is_some());
    }

    #[test]
    fn command_forms_extensions_and_literal_characters_are_closed() {
        let absolute =
            resolve_windows_command(&WindowsResolutionInput::new(units("C:\\Tools\\codex")))
                .unwrap();
        assert_eq!(absolute.command_form(), RuntimeCommandForm::WindowsAbsolute);
        assert_eq!(paths(&absolute), ["C:\\Tools\\codex.exe"]);

        let qualified = resolve_windows_command(
            &WindowsResolutionInput::new(units("tools\\codex.EXE"))
                .with_relative_base(Some(units("D:\\Work"))),
        )
        .unwrap();
        assert_eq!(
            qualified.command_form(),
            RuntimeCommandForm::WindowsQualified
        );
        assert_eq!(paths(&qualified), ["D:\\Work\\tools\\codex.EXE"]);
        let unproved =
            resolve_windows_command(&WindowsResolutionInput::new(units("tools\\codex"))).unwrap();
        assert!(unproved.path_unusable());
        let non_ascii_drive =
            resolve_windows_command(&WindowsResolutionInput::new(units("Ł:\\codex"))).unwrap();
        assert_eq!(
            non_ascii_drive.command_form(),
            RuntimeCommandForm::WindowsQualified
        );
        assert!(non_ascii_drive.path_unusable());

        for command in ["tool.bat", "tool.cmd", "tool.com"] {
            assert!(
                resolve_windows_command(&WindowsResolutionInput::new(units(command)))
                    .unwrap()
                    .path_unusable()
            );
        }
        let literal =
            resolve_windows_command(&WindowsResolutionInput::new(units("C:\\One\\a|b"))).unwrap();
        assert_eq!(paths(&literal), ["C:\\One\\a|b.exe"]);
    }

    #[test]
    fn bare_search_order_and_duplicate_entries_are_frozen() {
        let resolution = resolve_windows_command(
            &WindowsResolutionInput::new(units("codex"))
                .with_child_path(Some(units("C:\\One;C:\\One;C:\\Two")))
                .with_current_executable_dir(Some(units("C:\\App")))
                .with_system_dir(Some(units("C:\\System")))
                .with_windows_dir(Some(units("C:\\Windows")))
                .with_parent_path(Some(units("C:\\Parent"))),
        )
        .unwrap();
        assert_eq!(resolution.command_form(), RuntimeCommandForm::WindowsBare);
        assert_eq!(
            paths(&resolution),
            [
                "C:\\One\\codex.exe",
                "C:\\One\\codex.exe",
                "C:\\Two\\codex.exe",
                "C:\\App\\codex.exe",
                "C:\\System\\codex.exe",
                "C:\\Windows\\codex.exe",
                "C:\\Parent\\codex.exe",
            ]
        );
        assert_eq!(
            resolution.candidates()[0].candidate_digest(),
            resolution.candidates()[1].candidate_digest()
        );
    }

    #[test]
    fn relative_or_empty_search_entries_require_an_explicit_absolute_base() {
        for path in ["bin", ";C:\\Absolute"] {
            assert!(resolve_windows_command(
                &WindowsResolutionInput::new(units("codex")).with_child_path(Some(units(path))),
            )
            .unwrap()
            .path_unusable());
        }
        let resolved = resolve_windows_command(
            &WindowsResolutionInput::new(units("codex"))
                .with_child_path(Some(units("bin;")))
                .with_relative_base(Some(units("D:\\Stable")))
                .with_current_executable_dir(Some(units("C:\\App")))
                .with_system_dir(Some(units("C:\\System")))
                .with_windows_dir(Some(units("C:\\Windows"))),
        )
        .unwrap();
        assert_eq!(
            &paths(&resolved)[..2],
            ["D:\\Stable\\bin\\codex.exe", "D:\\Stable\\codex.exe"]
        );
    }

    #[test]
    fn every_windows_launch_input_limit_is_typed_before_resolution() {
        let over = vec![u16::from(b'x'); RUNTIME_FINGERPRINT_MAX_LAUNCH_INPUT_UNITS + 1];
        for (input, kind) in [
            (
                WindowsResolutionInput::new(over.clone()),
                RuntimeLaunchInputLimitKind::ConfiguredCommand,
            ),
            (
                WindowsResolutionInput::new(units("codex")).with_relative_base(Some(over.clone())),
                RuntimeLaunchInputLimitKind::WorkingDirectory,
            ),
            (
                WindowsResolutionInput::new(units("codex"))
                    .with_current_executable_dir(Some(over.clone())),
                RuntimeLaunchInputLimitKind::WindowsCurrentExecutableDirectory,
            ),
            (
                WindowsResolutionInput::new(units("codex")).with_system_dir(Some(over.clone())),
                RuntimeLaunchInputLimitKind::WindowsSystemDirectory,
            ),
            (
                WindowsResolutionInput::new(units("codex")).with_windows_dir(Some(over.clone())),
                RuntimeLaunchInputLimitKind::WindowsDirectory,
            ),
            (
                WindowsResolutionInput::new(units("codex")).with_parent_path(Some(over.clone())),
                RuntimeLaunchInputLimitKind::WindowsParentPath,
            ),
            (
                WindowsResolutionInput::new(units("codex")).with_child_path(Some(over)),
                RuntimeLaunchInputLimitKind::ChildPath,
            ),
        ] {
            assert!(matches!(
                resolve_windows_command(&input),
                Err(RuntimeFingerprintProduceError::LaunchInputLimitExceeded(observed))
                    if observed == kind
            ));
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_adapter_probe_helper() {
        use std::os::windows::ffi::OsStrExt;

        let Some(output_path) =
            harness_core::config::process_env::var_os("HARNESS_GH1733_ADAPTER_PROBE_OUTPUT")
        else {
            return;
        };
        let executable = std::env::current_exe().unwrap();
        let bytes = executable
            .as_os_str()
            .encode_wide()
            .flat_map(u16::to_le_bytes)
            .collect::<Vec<_>>();
        std::fs::write(output_path, bytes).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn frozen_bare_resolution_matches_the_current_adapter_command() {
        use std::ffi::{OsStr, OsString};
        use std::os::windows::ffi::{OsStrExt, OsStringExt};
        use std::path::{Path, PathBuf};

        fn wide(value: &OsStr) -> Vec<u16> {
            value.encode_wide().collect()
        }

        fn path_from_wide(value: &[u16]) -> PathBuf {
            PathBuf::from(OsString::from_wide(value))
        }

        let root = tempfile::tempdir().unwrap();
        let first = root.path().join("first path");
        let second = root.path().join("second & path");
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();
        let stem = format!("gh1733-adapter-probe-{}", std::process::id());
        let executable_name = format!("{stem}.exe");
        let source = std::env::current_exe().unwrap();
        let expected = first.join(&executable_name);
        std::fs::copy(&source, &expected).unwrap();
        std::fs::copy(&source, second.join(&executable_name)).unwrap();

        let child_path = std::env::join_paths([&first, &first, &second]).unwrap();
        let current_executable_dir = source.parent().map(Path::to_path_buf).unwrap();
        let windows_dir =
            harness_core::config::process_env::var_os("SystemRoot").map(PathBuf::from);
        let system_dir = windows_dir.as_ref().map(|path| path.join("System32"));
        let resolution = resolve_windows_command(
            &WindowsResolutionInput::new(units(&stem))
                .with_child_path(Some(wide(&child_path)))
                .with_relative_base(Some(wide(root.path().as_os_str())))
                .with_current_executable_dir(Some(wide(current_executable_dir.as_os_str())))
                .with_system_dir(system_dir.as_deref().map(|path| wide(path.as_os_str())))
                .with_windows_dir(windows_dir.as_deref().map(|path| wide(path.as_os_str()))),
        )
        .unwrap();
        let selected = resolution
            .candidates()
            .iter()
            .map(|candidate| path_from_wide(candidate.path()))
            .find(|candidate| candidate.exists())
            .unwrap();
        assert_eq!(
            std::fs::canonicalize(&selected).unwrap(),
            std::fs::canonicalize(&expected).unwrap()
        );

        let output_path = root.path().join("selected-path.bin");
        let status = std::process::Command::new(&stem)
            .args([
                "--exact",
                "runtime_fingerprint::windows_resolution::tests::windows_adapter_probe_helper",
            ])
            .current_dir(root.path())
            .env("PATH", child_path)
            .env("HARNESS_GH1733_ADAPTER_PROBE_OUTPUT", &output_path)
            .status()
            .unwrap();
        assert!(status.success());
        let bytes = std::fs::read(output_path).unwrap();
        assert_eq!(bytes.len() % 2, 0);
        let observed = bytes
            .chunks_exact(2)
            .map(|bytes| u16::from_le_bytes([bytes[0], bytes[1]]))
            .collect::<Vec<_>>();
        assert_eq!(
            std::fs::canonicalize(path_from_wide(&observed)).unwrap(),
            std::fs::canonicalize(selected).unwrap()
        );
    }
}
