use super::*;

fn units(value: &str) -> Vec<u16> {
    value.encode_utf16().collect()
}

fn utf16_string(value: &[u16]) -> String {
    match String::from_utf16(value) {
        Ok(value) => value,
        Err(error) => panic!("candidate path must be valid UTF-16: {error}"),
    }
}

fn resolved(input: &WindowsResolutionInput) -> WindowsResolution {
    match resolve_windows_command(input) {
        Ok(resolution) => resolution,
        Err(error) => panic!("Windows command resolution must succeed: {error}"),
    }
}

fn paths(resolution: &WindowsResolution) -> Vec<String> {
    resolution
        .candidates()
        .iter()
        .map(|candidate| utf16_string(candidate.path()))
        .collect()
}

fn with_special_directories(input: WindowsResolutionInput) -> WindowsResolutionInput {
    input
        .with_current_executable_dir(Some(units("C:\\App")))
        .with_system_dir(Some(units("C:\\Windows\\System32")))
        .with_windows_dir(Some(units("C:\\Windows")))
}

#[test]
fn configured_command_and_candidate_digest_vectors_are_independently_frozen() {
    let value = units("C:\\X");
    let resolution = resolved(&WindowsResolutionInput::new(value.clone()));
    assert_eq!(
        resolution.configured_command_digest().as_str(),
        "c10b026964d57087376ab987059507a7d943bdf07888ee9cc243cfb19f4094e5"
    );
    assert_eq!(
        digest_units(CANDIDATE_DOMAIN, &value).as_str(),
        "09be7aa3690d3a606adb29b6f70e199c3cabb8089fc81ee6ec1e3bab45886fe0"
    );
}

#[test]
fn every_windows_launch_input_accepts_exactly_the_unit_limit() {
    let exact = vec![u16::from(b'x'); RUNTIME_FINGERPRINT_MAX_LAUNCH_INPUT_UNITS];
    for input in [
        WindowsResolutionInput::new(exact.clone()),
        WindowsResolutionInput::new(units("codex")).with_relative_base(Some(exact.clone())),
        WindowsResolutionInput::new(units("codex"))
            .with_current_executable_dir(Some(exact.clone())),
        WindowsResolutionInput::new(units("codex")).with_system_dir(Some(exact.clone())),
        WindowsResolutionInput::new(units("codex")).with_windows_dir(Some(exact.clone())),
        WindowsResolutionInput::new(units("codex")).with_parent_path(Some(exact.clone())),
        WindowsResolutionInput::new(units("codex")).with_child_path(Some(exact)),
    ] {
        assert!(resolve_windows_command(&input).is_ok());
    }
}

#[test]
fn separator_dense_exact_limit_paths_do_not_materialize_candidate_paths() {
    let command = vec![u16::from(b'x'); RUNTIME_FINGERPRINT_MAX_LAUNCH_INPUT_UNITS];
    let path = vec![u16::from(b';'); RUNTIME_FINGERPRINT_MAX_LAUNCH_INPUT_UNITS];
    let resolution = resolved(&with_special_directories(
        WindowsResolutionInput::new(command)
            .with_relative_base(Some(units("C:\\")))
            .with_child_path(Some(path.clone()))
            .with_parent_path(Some(path)),
    ));
    assert_eq!(
        resolution.candidates().len(),
        2 * (RUNTIME_FINGERPRINT_MAX_LAUNCH_INPUT_UNITS + 1) + 3
    );
    assert!(resolution
        .candidates()
        .iter()
        .all(|candidate| !candidate.is_materialized()));
    assert_eq!(
        resolution.candidates()[0].path().len(),
        RUNTIME_FINGERPRINT_MAX_LAUNCH_INPUT_UNITS + 7
    );
    assert!(resolution.candidates()[0].is_materialized());
    assert!(!resolution.candidates()[1].is_materialized());
}

#[test]
fn bare_resolution_requires_every_special_directory() {
    let complete = with_special_directories(
        WindowsResolutionInput::new(units("codex")).with_child_path(Some(units("C:\\Tools"))),
    );
    assert!(!resolved(&complete).path_unusable());
    for incomplete in [
        complete.clone().with_current_executable_dir(None),
        complete.clone().with_system_dir(None),
        complete.with_windows_dir(None),
    ] {
        assert!(resolved(&incomplete).path_unusable());
    }
}

#[test]
fn root_relative_references_use_only_the_stable_drive_or_unc_root() {
    let drive = resolved(
        &WindowsResolutionInput::new(units("\\tools\\codex"))
            .with_relative_base(Some(units("D:\\Work\\Tree"))),
    );
    assert_eq!(paths(&drive), ["D:\\tools\\codex.exe"]);

    let unc = resolved(
        &WindowsResolutionInput::new(units("\\tools\\codex"))
            .with_relative_base(Some(units("\\\\server\\share\\Work"))),
    );
    assert_eq!(paths(&unc), ["\\\\server\\share\\tools\\codex.exe"]);

    let search = resolved(&with_special_directories(
        WindowsResolutionInput::new(units("codex"))
            .with_child_path(Some(units("\\tools")))
            .with_relative_base(Some(units("E:\\Work"))),
    ));
    assert_eq!(paths(&search)[0], "E:\\tools\\codex.exe");
}
