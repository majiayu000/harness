//! Native static ELF fixtures for Linux runtime supervision tests.

use std::path::{Path, PathBuf};

const LOAD_OFFSET: usize = 0x1000;
const LOAD_ADDRESS: u64 = 0x0040_0000;
const VERSION_OUTPUT: &[u8] = b"codex-cli 1.2.3\n";

struct ProgramHeader {
    kind: u32,
    flags: u32,
    offset: u64,
    address: u64,
    file_size: u64,
    memory_size: u64,
    alignment: u64,
}

pub(super) fn write_version_fixture(directory: &Path, name: &str) -> PathBuf {
    write_fixture(directory, name, version_code())
}

pub(super) fn write_loop_fixture(directory: &Path, name: &str) -> PathBuf {
    write_fixture(directory, name, loop_code())
}

fn write_fixture(directory: &Path, name: &str, code: Vec<u8>) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;

    let executable = directory.join(name);
    std::fs::write(&executable, elf_with_code(code)).expect("write static fixture");
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755))
        .expect("make static version fixture executable");
    executable
}

fn elf_with_code(code: Vec<u8>) -> Vec<u8> {
    let mut image = vec![0_u8; LOAD_OFFSET + code.len()];
    image[..4].copy_from_slice(b"\x7fELF");
    image[4] = 2;
    image[5] = 1;
    image[6] = 1;
    image[16..18].copy_from_slice(&2_u16.to_le_bytes());
    image[18..20].copy_from_slice(&native_machine().to_le_bytes());
    image[20..24].copy_from_slice(&1_u32.to_le_bytes());
    image[24..32].copy_from_slice(&(LOAD_ADDRESS + LOAD_OFFSET as u64).to_le_bytes());
    image[32..40].copy_from_slice(&64_u64.to_le_bytes());
    image[52..54].copy_from_slice(&64_u16.to_le_bytes());
    image[54..56].copy_from_slice(&56_u16.to_le_bytes());
    image[56..58].copy_from_slice(&2_u16.to_le_bytes());

    let file_size = image.len() as u64;
    write_program_header(
        &mut image[64..120],
        ProgramHeader {
            kind: 1,
            flags: 5,
            offset: 0,
            address: LOAD_ADDRESS,
            file_size,
            memory_size: file_size,
            alignment: 0x1000,
        },
    );
    write_program_header(
        &mut image[120..176],
        ProgramHeader {
            kind: 0x6474_e551,
            flags: 6,
            offset: 0,
            address: 0,
            file_size: 0,
            memory_size: 0,
            alignment: 16,
        },
    );
    image[LOAD_OFFSET..].copy_from_slice(&code);
    image
}

#[cfg(target_arch = "x86_64")]
fn loop_code() -> Vec<u8> {
    vec![0xeb, 0xfe]
}

#[cfg(target_arch = "aarch64")]
fn loop_code() -> Vec<u8> {
    0x1400_0000_u32.to_le_bytes().to_vec()
}

fn write_program_header(header: &mut [u8], value: ProgramHeader) {
    header[0..4].copy_from_slice(&value.kind.to_le_bytes());
    header[4..8].copy_from_slice(&value.flags.to_le_bytes());
    header[8..16].copy_from_slice(&value.offset.to_le_bytes());
    header[16..24].copy_from_slice(&value.address.to_le_bytes());
    header[24..32].copy_from_slice(&value.address.to_le_bytes());
    header[32..40].copy_from_slice(&value.file_size.to_le_bytes());
    header[40..48].copy_from_slice(&value.memory_size.to_le_bytes());
    header[48..56].copy_from_slice(&value.alignment.to_le_bytes());
}

#[cfg(target_arch = "x86_64")]
const fn native_machine() -> u16 {
    62
}

#[cfg(target_arch = "aarch64")]
const fn native_machine() -> u16 {
    183
}

#[cfg(target_arch = "x86_64")]
fn version_code() -> Vec<u8> {
    let mut code = vec![
        0xb8,
        0x01,
        0x00,
        0x00,
        0x00,
        0xbf,
        0x01,
        0x00,
        0x00,
        0x00,
        0x48,
        0x8d,
        0x35,
        0x10,
        0x00,
        0x00,
        0x00,
        0xba,
        VERSION_OUTPUT.len() as u8,
        0x00,
        0x00,
        0x00,
        0x0f,
        0x05,
        0xb8,
        0x3c,
        0x00,
        0x00,
        0x00,
        0x31,
        0xff,
        0x0f,
        0x05,
    ];
    code.extend_from_slice(VERSION_OUTPUT);
    code
}

#[test]
fn native_version_fixture_executes_without_an_interpreter() {
    let directory = tempfile::tempdir().expect("create fixture directory");
    let executable = write_version_fixture(directory.path(), "runtime-version");
    let output = std::process::Command::new(executable)
        .output()
        .expect("execute static version fixture");
    assert!(output.status.success());
    assert_eq!(output.stdout, VERSION_OUTPUT);
    assert!(output.stderr.is_empty());
}

#[cfg(target_arch = "aarch64")]
fn version_code() -> Vec<u8> {
    let instructions = [
        0xd280_0020_u32,
        0x1000_00e1,
        0xd280_0202,
        0xd280_0808,
        0xd400_0001,
        0xd280_0000,
        0xd280_0ba8,
        0xd400_0001,
    ];
    let mut code = Vec::with_capacity(instructions.len() * 4 + VERSION_OUTPUT.len());
    for instruction in instructions {
        code.extend_from_slice(&instruction.to_le_bytes());
    }
    code.extend_from_slice(VERSION_OUTPUT);
    code
}
