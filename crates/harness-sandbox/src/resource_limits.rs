use super::WrappedCommand;
use serde::{Deserialize, Serialize};
use std::ffi::OsString;
use std::fmt;
use std::path::{Path, PathBuf};
use thiserror::Error;

pub const EVAL_RESOURCE_LIMITS_CAPABILITY: &str = "eval_resource_limits";

const DEFAULT_MEMORY_BYTES: u64 = 8 * 1024 * 1024 * 1024;
const DEFAULT_PIDS: u64 = 512;
const DEFAULT_DISK_BYTES: u64 = 20 * 1024 * 1024 * 1024;
const DEFAULT_OUTPUT_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceLimits {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_time_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pids: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wall_time_secs: Option<u64>,
}

impl ResourceLimits {
    pub fn evaluation_defaults(timeout_secs: u64) -> Self {
        Self {
            cpu_time_secs: Some(timeout_secs),
            memory_bytes: Some(DEFAULT_MEMORY_BYTES),
            pids: Some(DEFAULT_PIDS),
            disk_bytes: Some(DEFAULT_DISK_BYTES),
            output_bytes: Some(DEFAULT_OUTPUT_BYTES),
            wall_time_secs: Some(timeout_secs),
        }
    }

    pub fn operator_default_maxima() -> Self {
        Self::evaluation_defaults(7_200)
    }

    pub fn overlay(self, overrides: Self) -> Self {
        Self {
            cpu_time_secs: overrides.cpu_time_secs.or(self.cpu_time_secs),
            memory_bytes: overrides.memory_bytes.or(self.memory_bytes),
            pids: overrides.pids.or(self.pids),
            disk_bytes: overrides.disk_bytes.or(self.disk_bytes),
            output_bytes: overrides.output_bytes.or(self.output_bytes),
            wall_time_secs: overrides.wall_time_secs.or(self.wall_time_secs),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.cpu_time_secs.is_none()
            && self.memory_bytes.is_none()
            && self.pids.is_none()
            && self.disk_bytes.is_none()
            && self.output_bytes.is_none()
            && self.wall_time_secs.is_none()
    }

    pub fn cap_by(self, maxima: Self) -> Result<CappedResourceLimits, ResourceLimitError> {
        self.validate_nonzero("requested")?;
        maxima.validate_nonzero("maximum")?;

        let mut effective = self;
        let mut caps = Vec::new();
        cap_field(
            ResourceLimitKind::CpuTime,
            self.cpu_time_secs,
            maxima.cpu_time_secs,
            &mut effective.cpu_time_secs,
            &mut caps,
        );
        cap_field(
            ResourceLimitKind::Memory,
            self.memory_bytes,
            maxima.memory_bytes,
            &mut effective.memory_bytes,
            &mut caps,
        );
        cap_field(
            ResourceLimitKind::Pids,
            self.pids,
            maxima.pids,
            &mut effective.pids,
            &mut caps,
        );
        cap_field(
            ResourceLimitKind::Disk,
            self.disk_bytes,
            maxima.disk_bytes,
            &mut effective.disk_bytes,
            &mut caps,
        );
        cap_field(
            ResourceLimitKind::Output,
            self.output_bytes,
            maxima.output_bytes,
            &mut effective.output_bytes,
            &mut caps,
        );
        cap_field(
            ResourceLimitKind::WallTime,
            self.wall_time_secs,
            maxima.wall_time_secs,
            &mut effective.wall_time_secs,
            &mut caps,
        );

        Ok(CappedResourceLimits {
            requested: self,
            effective,
            caps,
        })
    }

    fn validate_nonzero(&self, owner: &'static str) -> Result<(), ResourceLimitError> {
        for (kind, value) in self.fields() {
            if value == Some(0) {
                return Err(ResourceLimitError::InvalidLimit { owner, limit: kind });
            }
        }
        Ok(())
    }

    fn fields(&self) -> [(ResourceLimitKind, Option<u64>); 6] {
        [
            (ResourceLimitKind::CpuTime, self.cpu_time_secs),
            (ResourceLimitKind::Memory, self.memory_bytes),
            (ResourceLimitKind::Pids, self.pids),
            (ResourceLimitKind::Disk, self.disk_bytes),
            (ResourceLimitKind::Output, self.output_bytes),
            (ResourceLimitKind::WallTime, self.wall_time_secs),
        ]
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CappedResourceLimits {
    pub requested: ResourceLimits,
    pub effective: ResourceLimits,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub caps: Vec<ResourceLimitCap>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceLimitCap {
    pub resource: ResourceLimitKind,
    pub requested: u64,
    pub maximum: u64,
    pub effective: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourceLimitKind {
    CpuTime,
    Memory,
    Pids,
    Disk,
    Output,
    WallTime,
    UnknownQuota,
}

impl ResourceLimitKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::CpuTime => "cpu_time",
            Self::Memory => "memory",
            Self::Pids => "pids",
            Self::Disk => "disk",
            Self::Output => "output",
            Self::WallTime => "wall_time",
            Self::UnknownQuota => "unknown_quota",
        }
    }
}

impl fmt::Display for ResourceLimitKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceLimitBackend {
    UnixProcess,
    Unsupported,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceLimitReport {
    pub limits: CappedResourceLimits,
    pub usage: ResourceUsage,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub termination: Option<ResourceTermination>,
    pub reason: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceUsage {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_time_millis: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peak_memory_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peak_pids: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wall_time_millis: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceTermination {
    pub resource: ResourceLimitKind,
    pub reason: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResourceProcessStatus {
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ResourceLimitError {
    #[error("{owner} resource limit `{limit}` must be greater than zero")]
    InvalidLimit {
        owner: &'static str,
        limit: ResourceLimitKind,
    },
    #[error("resource limit backend `{backend}` cannot enforce `{limit}`")]
    UnsupportedBackend {
        backend: &'static str,
        limit: ResourceLimitKind,
    },
    #[error("output exceeded {limit_bytes} bytes after observing {observed_bytes} bytes")]
    OutputLimitExceeded {
        limit_bytes: u64,
        observed_bytes: u64,
    },
}

pub fn validate_resource_limit_backend(
    limits: &ResourceLimits,
    backend: ResourceLimitBackend,
) -> Result<(), ResourceLimitError> {
    if backend == ResourceLimitBackend::UnixProcess || limits.is_empty() {
        return Ok(());
    }
    let limit = limits
        .fields()
        .into_iter()
        .find_map(|(kind, value)| value.map(|_| kind))
        .unwrap_or(ResourceLimitKind::UnknownQuota);
    Err(ResourceLimitError::UnsupportedBackend {
        backend: "unsupported",
        limit,
    })
}

pub fn wrap_unix_command_with_resource_limits(
    program: &Path,
    args: &[OsString],
    limits: &ResourceLimits,
    timeout_program: Option<&Path>,
) -> Result<WrappedCommand, ResourceLimitError> {
    limits.validate_nonzero("requested")?;
    validate_resource_limit_backend(limits, ResourceLimitBackend::UnixProcess)?;

    if limits.is_empty() {
        return Ok(WrappedCommand {
            program: program.to_path_buf(),
            args: args.to_vec(),
            engine: super::SandboxEngine::None,
        });
    }

    if limits.wall_time_secs.is_some() && timeout_program.is_none() {
        return Err(ResourceLimitError::UnsupportedBackend {
            backend: "unix_process_without_timeout",
            limit: ResourceLimitKind::WallTime,
        });
    }

    let script = unix_resource_limit_script(limits, timeout_program.is_some());
    let mut wrapped_args = vec![
        OsString::from("-c"),
        OsString::from(script),
        OsString::from("harness-resource-limits"),
    ];
    if let Some(timeout_program) = timeout_program {
        wrapped_args.push(timeout_program.as_os_str().to_os_string());
    }
    wrapped_args.push(program.as_os_str().to_os_string());
    wrapped_args.extend(args.iter().cloned());

    Ok(WrappedCommand {
        program: PathBuf::from("/bin/sh"),
        args: wrapped_args,
        engine: super::SandboxEngine::None,
    })
}

pub fn classify_resource_termination(
    status: ResourceProcessStatus,
    limits: &ResourceLimits,
    output_limit_exceeded: bool,
) -> Option<ResourceTermination> {
    if output_limit_exceeded {
        return Some(ResourceTermination {
            resource: ResourceLimitKind::Output,
            reason: "output limit exceeded".to_string(),
        });
    }
    if status.exit_code == Some(124) && limits.wall_time_secs.is_some() {
        return Some(ResourceTermination {
            resource: ResourceLimitKind::WallTime,
            reason: "wall-clock timeout exceeded".to_string(),
        });
    }
    match status.signal {
        #[cfg(unix)]
        Some(signal) if signal == libc::SIGXCPU && limits.cpu_time_secs.is_some() => {
            Some(ResourceTermination {
                resource: ResourceLimitKind::CpuTime,
                reason: "CPU time limit exceeded".to_string(),
            })
        }
        #[cfg(unix)]
        Some(signal) if signal == libc::SIGXFSZ && limits.disk_bytes.is_some() => {
            Some(ResourceTermination {
                resource: ResourceLimitKind::Disk,
                reason: "file size or disk quota exceeded".to_string(),
            })
        }
        #[cfg(unix)]
        Some(signal)
            if matches!(signal, libc::SIGKILL | libc::SIGTERM)
                && (limits.memory_bytes.is_some()
                    || limits.pids.is_some()
                    || limits.cpu_time_secs.is_some()
                    || limits.wall_time_secs.is_some()) =>
        {
            Some(ResourceTermination {
                resource: ResourceLimitKind::UnknownQuota,
                reason: "process was killed while resource limits were active".to_string(),
            })
        }
        _ => None,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputLimitTracker {
    limit_bytes: u64,
    observed_bytes: u64,
}

impl OutputLimitTracker {
    pub fn new(limit_bytes: u64) -> Result<Self, ResourceLimitError> {
        if limit_bytes == 0 {
            return Err(ResourceLimitError::InvalidLimit {
                owner: "requested",
                limit: ResourceLimitKind::Output,
            });
        }
        Ok(Self {
            limit_bytes,
            observed_bytes: 0,
        })
    }

    pub fn observe_chunk(&mut self, chunk: &[u8]) -> Result<(), ResourceLimitError> {
        let observed = self.observed_bytes.saturating_add(chunk.len() as u64);
        if observed > self.limit_bytes {
            return Err(ResourceLimitError::OutputLimitExceeded {
                limit_bytes: self.limit_bytes,
                observed_bytes: observed,
            });
        }
        self.observed_bytes = observed;
        Ok(())
    }

    pub fn observed_bytes(&self) -> u64 {
        self.observed_bytes
    }
}

fn cap_field(
    resource: ResourceLimitKind,
    requested: Option<u64>,
    maximum: Option<u64>,
    effective: &mut Option<u64>,
    caps: &mut Vec<ResourceLimitCap>,
) {
    let (Some(requested), Some(maximum)) = (requested, maximum) else {
        return;
    };
    if requested <= maximum {
        return;
    }
    *effective = Some(maximum);
    caps.push(ResourceLimitCap {
        resource,
        requested,
        maximum,
        effective: maximum,
    });
}

fn unix_resource_limit_script(limits: &ResourceLimits, use_timeout: bool) -> String {
    let mut lines = Vec::new();
    if let Some(cpu_time_secs) = limits.cpu_time_secs {
        lines.push(format!("ulimit -t {cpu_time_secs}"));
    }
    if let Some(memory_bytes) = limits.memory_bytes {
        lines.push(format!("ulimit -v {}", ceil_div(memory_bytes, 1024)));
    }
    if let Some(pids) = limits.pids {
        lines.push(format!("ulimit -u {pids}"));
    }
    if let Some(disk_bytes) = limits.disk_bytes {
        lines.push(format!("ulimit -f {}", ceil_div(disk_bytes, 512)));
    }
    if use_timeout {
        let wall_time_secs = limits.wall_time_secs.expect("timeout requested");
        lines.push("timeout_bin=$1".to_string());
        lines.push("shift".to_string());
        lines.push(format!(
            "exec \"$timeout_bin\" --kill-after=5s {wall_time_secs}s \"$@\""
        ));
    } else {
        lines.push("exec \"$@\"".to_string());
    }
    lines.join("\n")
}

fn ceil_div(value: u64, divisor: u64) -> u64 {
    value.saturating_add(divisor.saturating_sub(1)) / divisor
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resource_limits_cap_requests_by_operator_maxima() {
        let requested = ResourceLimits {
            cpu_time_secs: Some(120),
            memory_bytes: Some(2_048),
            pids: Some(64),
            disk_bytes: Some(4_096),
            output_bytes: Some(2_048),
            wall_time_secs: Some(180),
        };
        let maxima = ResourceLimits {
            cpu_time_secs: Some(60),
            memory_bytes: Some(1_024),
            pids: Some(128),
            disk_bytes: Some(2_048),
            output_bytes: Some(1_024),
            wall_time_secs: Some(90),
        };

        let capped = requested.cap_by(maxima).expect("limits should cap");

        assert_eq!(capped.effective.cpu_time_secs, Some(60));
        assert_eq!(capped.effective.memory_bytes, Some(1_024));
        assert_eq!(capped.effective.pids, Some(64));
        assert_eq!(capped.effective.disk_bytes, Some(2_048));
        assert_eq!(capped.effective.output_bytes, Some(1_024));
        assert_eq!(capped.effective.wall_time_secs, Some(90));
        assert_eq!(capped.caps.len(), 5);
    }

    #[test]
    fn resource_limits_reject_zero_values() {
        let err = ResourceLimits {
            cpu_time_secs: Some(0),
            ..ResourceLimits::default()
        }
        .cap_by(ResourceLimits::operator_default_maxima())
        .expect_err("zero limit should fail closed");

        assert!(matches!(
            err,
            ResourceLimitError::InvalidLimit {
                limit: ResourceLimitKind::CpuTime,
                ..
            }
        ));
    }

    #[test]
    fn resource_limits_report_unsupported_backend() {
        let limits = ResourceLimits {
            memory_bytes: Some(1024),
            ..ResourceLimits::default()
        };

        let err = validate_resource_limit_backend(&limits, ResourceLimitBackend::Unsupported)
            .expect_err("unsupported backend should be rejected");

        assert!(matches!(
            err,
            ResourceLimitError::UnsupportedBackend {
                limit: ResourceLimitKind::Memory,
                ..
            }
        ));
    }

    #[test]
    fn resource_limits_build_unix_wrapper_with_process_limits() {
        let limits = ResourceLimits {
            cpu_time_secs: Some(9),
            memory_bytes: Some(2049),
            pids: Some(32),
            disk_bytes: Some(1025),
            output_bytes: Some(4096),
            wall_time_secs: Some(11),
        };
        let wrapped = wrap_unix_command_with_resource_limits(
            Path::new("/usr/bin/env"),
            &[OsString::from("true")],
            &limits,
            Some(Path::new("/usr/bin/timeout")),
        )
        .expect("wrapper should build");
        let script = wrapped.args[1].to_string_lossy();

        assert_eq!(wrapped.program, PathBuf::from("/bin/sh"));
        assert!(script.contains("ulimit -t 9"));
        assert!(script.contains("ulimit -v 3"));
        assert!(script.contains("ulimit -u 32"));
        assert!(script.contains("ulimit -f 3"));
        assert!(script.contains("exec \"$timeout_bin\" --kill-after=5s 11s \"$@\""));
        assert_eq!(
            wrapped.args[3..],
            [
                OsString::from("/usr/bin/timeout"),
                OsString::from("/usr/bin/env"),
                OsString::from("true")
            ]
        );
    }

    #[test]
    fn resource_limits_require_timeout_backend_for_wall_time() {
        let limits = ResourceLimits {
            wall_time_secs: Some(11),
            ..ResourceLimits::default()
        };

        let err =
            wrap_unix_command_with_resource_limits(Path::new("/bin/true"), &[], &limits, None)
                .expect_err("missing timeout backend should fail");

        assert!(matches!(
            err,
            ResourceLimitError::UnsupportedBackend {
                limit: ResourceLimitKind::WallTime,
                ..
            }
        ));
    }

    #[test]
    fn resource_limits_track_output_limit() {
        let mut tracker = OutputLimitTracker::new(4).expect("valid limit");

        tracker.observe_chunk(b"ab").expect("under limit");
        let err = tracker
            .observe_chunk(b"cde")
            .expect_err("chunk crossing the limit should fail");

        assert_eq!(tracker.observed_bytes(), 2);
        assert!(matches!(
            err,
            ResourceLimitError::OutputLimitExceeded {
                limit_bytes: 4,
                observed_bytes: 5
            }
        ));
    }

    #[test]
    fn resource_limits_classify_quota_termination_separately() {
        let limits = ResourceLimits::evaluation_defaults(30);

        let wall = classify_resource_termination(
            ResourceProcessStatus {
                exit_code: Some(124),
                signal: None,
            },
            &limits,
            false,
        )
        .expect("timeout should classify");
        assert_eq!(wall.resource, ResourceLimitKind::WallTime);

        let output = classify_resource_termination(
            ResourceProcessStatus {
                exit_code: Some(1),
                signal: None,
            },
            &limits,
            true,
        )
        .expect("output limit should classify");
        assert_eq!(output.resource, ResourceLimitKind::Output);

        let normal = classify_resource_termination(
            ResourceProcessStatus {
                exit_code: Some(1),
                signal: None,
            },
            &limits,
            false,
        );
        assert_eq!(normal, None);
    }
}
