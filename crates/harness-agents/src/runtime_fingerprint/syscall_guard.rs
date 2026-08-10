//! Frozen Linux syscall-entry parsing and transitive execution policy.

use harness_core::stack::fingerprint::RuntimeProbeFailureDetail;

const PTRACE_SYSCALL_INFO_ENTRY: u8 = 1;
const PTRACE_SYSCALL_INFO_EXIT: u8 = 2;
#[cfg(target_arch = "x86_64")]
const AUDIT_ARCH_X86_64: u32 = 0xc000_003e;
#[cfg(target_arch = "aarch64")]
const AUDIT_ARCH_AARCH64: u32 = 0xc000_00b7;
#[cfg(target_arch = "x86_64")]
const X32_SYSCALL_BIT: u64 = 0x4000_0000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SyscallStop {
    Entry { number: u64, arguments: [u64; 6] },
    Exit,
}

pub(super) fn read_syscall_stop(pid: libc::pid_t) -> Option<SyscallStop> {
    let mut info = [0_u8; 128];
    let size = unsafe {
        super::probe::ptrace(
            libc::PTRACE_GET_SYSCALL_INFO,
            pid,
            super::probe::ptrace_word(info.len()),
            info.as_mut_ptr().cast(),
        )
    };
    if size < 24 || read_u32(&info, 4) != expected_audit_arch() {
        return None;
    }
    match info[0] {
        PTRACE_SYSCALL_INFO_ENTRY if size >= 80 => {
            let mut arguments = [0_u64; 6];
            for (index, argument) in arguments.iter_mut().enumerate() {
                *argument = read_u64(&info, 32 + index * 8);
            }
            Some(SyscallStop::Entry {
                number: read_u64(&info, 24),
                arguments,
            })
        }
        PTRACE_SYSCALL_INFO_EXIT if size >= 33 => Some(SyscallStop::Exit),
        _ => None,
    }
}

pub(super) fn denied_class(
    number: u64,
    arguments: [u64; 6],
) -> Result<Option<RuntimeProbeFailureDetail>, ()> {
    #[cfg(target_arch = "x86_64")]
    if number & X32_SYSCALL_BIT != 0 {
        return Err(());
    }
    Ok(classify_native(number, arguments))
}

pub(super) const fn is_exit_syscall(number: u64) -> bool {
    #[cfg(target_arch = "x86_64")]
    {
        matches!(number, 60 | 231)
    }
    #[cfg(target_arch = "aarch64")]
    {
        matches!(number, 93 | 94)
    }
}

#[cfg(target_arch = "x86_64")]
fn classify_native(number: u64, arguments: [u64; 6]) -> Option<RuntimeProbeFailureDetail> {
    use RuntimeProbeFailureDetail as D;
    if matches!(number, 56 | 57 | 58 | 435) {
        return Some(D::ProcessCreation);
    }
    if matches!(number, 59 | 322) {
        return Some(D::ImageExecution);
    }
    if matches!(number, 9 | 10 | 329) && arguments[2] & libc::PROT_EXEC as u64 != 0
        || number == 30 && arguments[2] & libc::SHM_EXEC as u64 != 0
        || number == 134
    {
        return Some(D::ExecutableMapping);
    }
    if matches!(
        number,
        101 | 311 | 323 | 425 | 438 | 47 | 299 | 157 | 85 | 437
    ) || number == 135 && arguments[0] != u64::from(u32::MAX)
        || number == 2 && write_capable(arguments[1])
        || matches!(number, 257 | 304) && write_capable(arguments[2])
    {
        return Some(D::ExecutableImageMutation);
    }
    if matches!(number, 175 | 313 | 321) {
        return Some(D::KernelCodeLoading);
    }
    if matches!(number, 62 | 129 | 200 | 234 | 297 | 424) {
        return Some(D::ProcessSignalling);
    }
    None
}

#[cfg(target_arch = "aarch64")]
fn classify_native(number: u64, arguments: [u64; 6]) -> Option<RuntimeProbeFailureDetail> {
    use RuntimeProbeFailureDetail as D;
    if matches!(number, 220 | 435) {
        return Some(D::ProcessCreation);
    }
    if matches!(number, 221 | 281) {
        return Some(D::ImageExecution);
    }
    if matches!(number, 222 | 226 | 288) && arguments[2] & libc::PROT_EXEC as u64 != 0
        || number == 196 && arguments[2] & libc::SHM_EXEC as u64 != 0
    {
        return Some(D::ExecutableMapping);
    }
    if matches!(number, 117 | 271 | 282 | 425 | 438 | 212 | 243 | 167 | 437)
        || number == 92 && arguments[0] != u64::from(u32::MAX)
        || matches!(number, 56 | 265) && write_capable(arguments[2])
    {
        return Some(D::ExecutableImageMutation);
    }
    if matches!(number, 105 | 273 | 280) {
        return Some(D::KernelCodeLoading);
    }
    if matches!(number, 129 | 130 | 131 | 138 | 240 | 424) {
        return Some(D::ProcessSignalling);
    }
    None
}

fn write_capable(flags: u64) -> bool {
    let access = flags & libc::O_ACCMODE as u64;
    access == libc::O_WRONLY as u64
        || access == libc::O_RDWR as u64
        || flags & libc::O_TRUNC as u64 != 0
}

fn expected_audit_arch() -> u32 {
    #[cfg(target_arch = "x86_64")]
    {
        AUDIT_ARCH_X86_64
    }
    #[cfg(target_arch = "aarch64")]
    {
        AUDIT_ARCH_AARCH64
    }
}

fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_ne_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
    ])
}

fn read_u64(bytes: &[u8], offset: usize) -> u64 {
    u64::from_ne_bytes([
        bytes[offset],
        bytes[offset + 1],
        bytes[offset + 2],
        bytes[offset + 3],
        bytes[offset + 4],
        bytes[offset + 5],
        bytes[offset + 6],
        bytes[offset + 7],
    ])
}
