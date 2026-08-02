# Runtime Observation Design

## Linked Spec Packet

This document defines GH-1733 runtime command resolution, retained-handle
observation, executable authorization, and TOCTOU controls. Read it with
`product.md`, `runtime-product.md`, `runtime-supervision.md`, `tech.md`, and
`tasks.md`.

## Single-Command PATH Resolution

`executable.rs` resolves one configured command without a shell. Windows and
Unix each freeze the explicit v0.1 algorithm below independently of the
compiler used to build Harness. The input carries a typed
`RuntimeLaunchContext`: platform, configured child working directory,
sanitized child `PATH`, and every platform search base that the resolver needs
but must not infer. It also carries the validated repository boundary set used
only for B-007 target authorization; an absent or incomplete set can still
produce identity evidence but can never authorize process creation.

For a platform admitted by the static matrix, immediately after the isolation,
sandbox, and public `max_output_bytes` gates, and before hashing, splitting,
joining, owner admission, cwd observation, or child creation, the producer
validates every launch string with bounded iteration through at most
limit-plus-one OS units. `RUNTIME_FINGERPRINT_MAX_LAUNCH_INPUT_UNITS = 65_536`
is the inclusive limit for each configured command, configured working-
directory spelling, sanitized child `PATH`, policy-selected
`CLAUDE_CONFIG_DIR` after setup-secret exclusion, and each present Windows
current-executable directory, system directory, Windows directory, and parent
`PATH`. A Unix unit is one exact `OsStrExt::as_bytes()` byte; a Windows unit is
one original `encode_wide()` UTF-16 code unit.
`RUNTIME_FINGERPRINT_MAX_OBSERVATION_ENV_ENTRIES = 1_024`,
`RUNTIME_FINGERPRINT_MAX_ENVIRONMENT_KEY_UNITS = 1_024`,
`RUNTIME_FINGERPRINT_MAX_SETUP_SECRET_NAMES = 1_024`, and
`RUNTIME_FINGERPRINT_MAX_SETUP_SECRET_NAME_UNITS = 1_024` bound the two
producer-local name collections before canonicalization or value access.
Those validated fields prove that every derived lexical candidate is at most
196,610 units: 65,536 cwd units + one separator + 65,536 relative PATH-entry
units plus one separator plus 65,536 command units. There is no separate
caller-reachable derived-candidate limit error.

The closed `RuntimeLaunchInputLimitKind` values are `ConfiguredCommand`,
`WorkingDirectory`, `WindowsCurrentExecutableDirectory`,
`WindowsSystemDirectory`, `WindowsDirectory`, `WindowsParentPath`,
`ObservationEnvironmentEntries`, `EnvironmentKey`, `SetupSecretNames`,
`SetupSecretName`, `ChildPath`, and `ClaudeConfigDirectory`. Exceeding one returns no-envelope
`LaunchInputLimitExceeded { kind }` with no raw value, digest, owner, fd, cwd
open, helper, exec, or PATH fallback.

Validation precedence is exactly isolation, sandbox, the actual static
unsupported-platform gate, public `max_output_bytes` range, configured command,
working directory, and present explicit Windows search-base limits,
observation-environment entry count then its key limits, setup-secret name
count then its name limits, key shape/canonicalization/collision checks, setup-secret
exclusion, selected `PATH` and `CLAUDE_CONFIG_DIR` value limits, empty/NUL and
launch-context shape validation, hashing, then owner-capacity admission. The
bounded Unix `PATH` is traversed lazily only after admission; entry count is not
an eager launch-input gate. Every reached join uses checked arithmetic and
allocates no more than the proven 196,610-unit maximum. Within each collection its count precedes its per-name checks, and
per-name checks use input order. Pure typed digest/model helpers apply every relevant limit before
hashing even in cross-platform contract tests.
Inputs are never truncated, normalized, or prefix-hashed. This bounded
producer may therefore reject an over-limit launch the adapter could attempt;
that is an explicit representability divergence, not another candidate
selection.

The pure command-form model has exactly `UnixBare`, `UnixAbsolute`,
`UnixQualified`, `WindowsBare`, `WindowsAbsolute`, or `WindowsQualified`. A
v0.1 envelope payload records only one of the three Unix forms. Every Windows
form is available only to pure resolver/digest helpers and is rejected by the
envelope constructor and parser. For admitted Unix evidence, the producer
derives the form from the original configured `OsStr`; the parser uses it to
validate search versus configured-path outcomes without serializing the
command.

Before resolution, the producer computes `configured_command_digest` from a
domain-separated exact OS-string representation:

```text
"harness_runtime_configured_command_v0_1\0"
  || platform_tag
  || unit_count_be
  || exact_units
```

`platform_tag` is exactly `b"unix\0"` or `b"windows\0"`.
`unit_count_be` is an unsigned fixed-width `u64` in big-endian order. Unix
counts and appends `OsStrExt::as_bytes()` unchanged. Windows counts original
`encode_wide()` code units and appends each as little-endian `u16`. No UTF-8 conversion,
case-folding, slash normalization, dot-segment folding, absolutization, or
symlink resolution occurs. Empty or NUL-containing commands fail typed before
hashing. Only the admitted Unix digest is a payload fact and enters
`fingerprint_digest`; the pure Windows digest is helper-only and never enters a
v0.1 envelope. The raw command is never serialized and the value never becomes
ASC-001 integrity.

The configured child working-directory spelling uses the same helper with
domain
`b"harness_runtime_working_directory_v0_1\0"`. The admitted Unix digest enters
the payload as `working_directory_digest`; the pure Windows helper vector never
enters a v0.1 envelope. After the isolation and sandbox gates, Linux v0.1
reserves the runtime-independent owner, uses one reserved slot for owner-side
`pidfd_open(getpid(), 0)` plus `pidfd_send_signal(..., 0)`, closes that test
pidfd, and only then creates the capability child. Any self-preflight syscall
failure returns no-envelope `ContainmentUnavailable(PidfdUnavailable)` before
cwd observation; it cannot be reported as a capability-child registration
failure. This proves open and signalling only. Under the already-started
active deadline, the capability child then proves strong stopped-process/image
observation, tagged ptrace guarding, the kill-isolated observation protocol,
and exact pidfd wait/reap before containment becomes available. macOS, other Unix,
and Windows return typed no-envelope producer error
`ContainmentUnavailable(UnsupportedPlatform)` before cwd observation. On
supported Linux, an observation subprocess opens the directory once with
`O_PATH | O_DIRECTORY | O_CLOEXEC`, returns the descriptor over the private
fixed-frame socket using `SCM_RIGHTS`, and the parent retains it for every later
observation and target helper. Authoritative directory handle metadata is
required. `O_NOFOLLOW` is deliberately absent so a configured cwd symlink is
resolved once and then pinned by the retained handle. Execute/search-only
directories that deny ordinary read access remain usable when `O_PATH` and
`fchdir` succeed; exact tests cover that mode and cwd pathname replacement. It
records

```text
SHA-256(
  "harness_runtime_working_directory_identity_v0_1\0"
  || u64be(st_dev)
  || u64be(st_ino)
)
```

as `working_directory_identity_digest`. Open or metadata failure is typed
producer-input `WorkingDirectoryUnavailable` and emits no envelope. Raw path,
device, and inode never enter evidence.

Independent vectors are exact: Unix working-directory bytes `/x` hash to
`bdc1de448a5df96390bcc54bf757c96abf628c534baef27bdeba60c5350ebaf6`;
Windows UTF-16 units for `C:\X` hash to
`90e7e9eb468b08a8b8b5161fb2211bcba076a30439db72f7d6761d6398372085`;
and Unix `st_dev = 1`, `st_ino = 2` hash to
`0980191ed8a4adfd1d3a83af85fb72a46b9aae6ff342d53517995d161ee7f4f9`.

The first operations are the isolation and sandbox gates above. Their ordering
is a tested privacy and evidence boundary, not merely an unsupported branch
later in the probe.

On Unix:

- an absolute path becomes the sole
  `RuntimeResolvedCandidate::Absolute` reference;
- a qualified relative path becomes a
  `RuntimeResolvedCandidate::WorkingDirectoryRelative` reference;
- a bare name traverses sanitized `PATH` in order, with empty and relative
  entries producing working-directory-relative references and absolute entries
  producing absolute references;
- every preliminary observation and authoritative open of a
  working-directory-relative reference uses `fstatat`/`openat` (or an exact
  handle-relative equivalent) against the one retained working-directory
  descriptor; no access reconstructs an absolute pathname;
- a bare name with sanitized child `PATH = Unset` is `path_unusable` before
  candidate observation; v0.1 does not guess libc's platform default, while
  absolute and qualified commands remain representable;
- for `UnixBare`, missing, non-regular, and mode-ineligible same-basename
  candidates advance without process creation;
- for `UnixAbsolute` or `UnixQualified`, an exact missing result is terminal
  `path_not_found`, while an existing non-regular or mode-ineligible candidate
  is terminal and requires `identity/not_regular_file` or
  `identity/not_executable`;
- no more than 64 sanitized PATH entries are observed; reaching entry 65
  before a terminal outcome emits `candidate_limit_exceeded`, but an earlier
  terminal outcome never inspects or counts the remaining bounded PATH;
- exact `ENOENT` or `ENOTDIR` from authoritative open is `Absent`, just like
  the preliminary observation: a bare search advances and an
  absolute/qualified sole candidate ends as `path_not_found`;
- every other failure to open an otherwise statically eligible candidate stops
  with `identity/open_failed`, because skipping an unreadable execute-only
  candidate could select a different executable than the adapter;
- after handle inspection and the pre-spawn checkpoint, `probe.rs` maps the
  retained authorized handle collision-safely to the fixed child descriptor
  `RUNTIME_FINGERPRINT_TARGET_EXEC_FD = 10`, keeps `FD_CLOEXEC`, and uses direct
  `execveat(10, "", ..., AT_EMPTY_PATH)` from the existing workspace `libc` dependency
  under a `PTRACE_O_TRACEEXEC | PTRACE_O_TRACESYSGOOD` parent trace, with fixed
  arguments, post-exec syscall-stop guard, and no shell;
  interpreter-script exec fails before interpreter execution because the script
  descriptor closes on exec; a successful native exec stops at
  `PTRACE_EVENT_EXEC` before its first instruction, and only matching stopped-
  image strong identity plus a retained-handle re-hash under kernel write denial
  allows resume; `argv[0]` retains the exact original configured command
  `OsStr` units used by the adapter; the Linux kernel supplies exact
  `AT_EXECFN = "/dev/fd/10"` and closes fd 10 in the new image, so the closed
  `RuntimeExecContext::LinuxFdCloexecExecveatEmptyPathFd10` is required on
  every direct-exec attempt and enters the fingerprint;
- path-based `execve` after authorization is forbidden; missing ptrace
  exec-stop or post-exec syscall guarding is a pre-observation no-envelope
  containment error; after those gates pass, exact `ENOSYS`, `EPERM`, or
  `EINVAL` from the fully frozen target
  `execveat(10, "", ..., AT_EMPTY_PATH)` records
  `handle_execution_unavailable`, reaps every created target helper, and never
  falls back to the pathname;
- only exact `EACCES` from that call advances to the next same-basename
  candidate;
- exact first `ETXTBSY` waits the adapter's fixed 150 milliseconds, repeats
  opened-target authorization plus the full retained-handle hash and path
  strong-identity checkpoint, and retries the same retained authorized handle
  once; the retained candidate reference and its lexical digest remain
  resolution evidence only;
  identity change stops without retry; exact `ENOENT`/`ENOTDIR` on either the
  first or second target exec is terminal
  `interpreter_authorization_unavailable`; exact `ENOSYS`/`EPERM`/`EINVAL` on
  either call is terminal `handle_execution_unavailable`; second `ETXTBSY`,
  `ENOEXEC`, and every remaining error are terminal `spawn_failed`;
- absolute and qualified commands may use that one same-candidate `ETXTBSY`
  retry but never search or fallback to another path; and
- the first successful exec becomes the selected executable.

The implementation must not call `execvp`: POSIX `ENOEXEC` fallback may invoke
`/bin/sh`, which violates the no-shell boundary. Every observed Unix candidate
adds one ordered `RuntimeResolutionAttempt`; absolute and qualified commands
have exactly one, while a bare name has at most 64. Its digest is:

```text
SHA-256(
  "harness_runtime_resolution_candidate_v0_1\0"
  || platform_tag
  || unit_count_be
  || exact_units
)
```

The tags, counts, and exact units are encoded identically to
`configured_command_digest`. Each attempt also carries a closed
`RuntimeExecSequence`: `None`, `Single`, or
`EtxtbsyThenCheckpointAfter150Ms`, plus optional
`RuntimeExecContext`. `None` requires no context; `Single` and the retry
sequence require exactly
`LinuxFdCloexecExecveatEmptyPathFd10`. The closed
`RuntimeResolutionAttemptOutcome` is exactly `Absent`, `NotRegular`,
`NotExecutable`, `InspectionFailed`, `InspectionTarget`,
`AuthorizationUnavailable`, `InterpreterAuthorizationUnavailable`,
`HandleExecutionUnavailable`, `RetryNotAuthorized`,
`RetryAuthorizationUnavailable`, `SupervisionSetupFailed`, `ExecEacces`,
`ExecVerificationFailed`, `ExecFailed`, or `ExecStarted`.
`InterpreterAuthorizationUnavailable` is terminal, requires
`version_probe/interpreter_authorization_unavailable`, and uses
`RuntimeExecSequence::None` when the bounded pre-target classifier observes a
script, dynamic or malformed ELF, wrong-architecture ELF, or any other
unsupported format, or `Single` / `EtxtbsyThenCheckpointAfter150Ms` for exact
exec-time `ENOENT`/`ENOTDIR`; every form proves no target/loader/interpreter instruction
ran and any setup helper for that attempt was reaped. A `None` result's
no-target-helper fact is candidate-local and does not erase an earlier
ordered, fully reaped `ExecEacces` attempt.
`ExecVerificationFailed` requires
`identity/identity_changed`, uses `Single` or the retry sequence, and proves the
exec-stopped child was killed/reaped without resume.
`SupervisionSetupFailed` is
terminal, requires
the matching version-probe failure, uses `RuntimeExecSequence::None` for initial
target setup or `EtxtbsyThenCheckpointAfter150Ms` for the second target
after an exact first `ETXTBSY`, and proves every created helper was reaped before
the affected target handle-exec call. Attempts preserve PATH order and duplicates; they
are never sorted.

For an absolute candidate, digest units are its exact absolute spelling. For a
working-directory-relative candidate, digest units are the lexical join of the
configured working-directory spelling and exact relative reference, without
normalization. That lexical value is evidence only and is never passed to an
access API; `working_directory_identity_digest` plus handle-relative
`fstatat`/`openat` binds the actual directory used.

The parser enforces a finite-state contract together with
`RuntimeCommandForm`. Under `UnixBare`, `Absent`, `NotRegular`, and
`NotExecutable` are skips and may precede `ExecEacces` or one terminal outcome.
Under `UnixAbsolute` or `UnixQualified`, exactly one attempt is required:
`Absent` terminates as `path_not_found`, while `NotRegular` and
`NotExecutable` are terminal and require their matching identity failure.
`InspectionFailed` requires the matching
identity failure. `InspectionTarget` is permitted only with
`probe_not_authorized` and one closed configuration-source or resolved-target
repository reason, and forbids all exec outcomes. `AuthorizationUnavailable`
requires exactly `target_authorization_unavailable` carrying
`BoundaryUnprovable`, `LinkCountUnprovable`, `UnlinkedTarget`, or
`MultipleHardLinks`, is terminal, and forbids exec, fallback, or selected
identity.
`InterpreterAuthorizationUnavailable`
requires exactly the matching failure and its sequence distinguishes
pre-observed unsupported format from exact exec-time `ENOENT`/`ENOTDIR`; both forbid fallback and
selected identity. `ExecVerificationFailed` requires `identity_changed`,
forbids resume/fallback/selected identity, and proves the stopped child was
reaped.
`HandleExecutionUnavailable` requires
exactly `handle_execution_unavailable`, is terminal, uses `Single` for the
first call or `EtxtbsyThenCheckpointAfter150Ms` for the second, always with
`LinuxFdCloexecExecveatEmptyPathFd10`, and forbids path fallback and selected
identity. It is legal only when mandatory ptrace containment passed and the
fully frozen target call returned exact `ENOSYS`, `EPERM`, or `EINVAL`; every
created target helper must be reaped and no target instruction may run. `ExecFailed`
requires `spawn_failed`; `ExecStarted` forbids a spawn failure and is the only
outcome that creates a selected/executed identity. A sequence containing only
skips yields `path_not_found`; one ending in `ExecEacces` with no final identity
yields `bare_eacces_exhausted`; reaching the bound without a terminal outcome
yields `candidate_limit_exceeded` with exactly 64 attempts. Outcomes after a
terminal, multiple terminals, wrong source/failure pairs, or more than 64
entries fail parsing. Every Windows command form and every present Windows
resolution context fails v0.1 envelope construction and parsing before attempt
validation; its pure resolver/digest value is not envelope evidence.

`RuntimeExecSequence::None` is required for skips, inspection-only,
pre-observed unsupported-format interpreter-authorization-unavailable,
initial authorization-unavailable, and initial
supervision-setup-failed outcomes, and requires absent `RuntimeExecContext`.
`Single` is required for an ordinary exec outcome, exact exec-time interpreter
failure, initial handle-execution-unavailable, or exec-verification failure, and requires
`LinuxFdCloexecExecveatEmptyPathFd10`. The retry sequence requires the same
context.
`EtxtbsyThenCheckpointAfter150Ms` is legal only after the first direct
exec returned raw errno `ETXTBSY`; it requires the 150-millisecond monotonic
delay and the repeated authorization/hash/path-identity checkpoint. If that
checkpoint changes authorization, `RetryNotAuthorized` requires
`probe_not_authorized` with exact `ResolvedTargetRepository` reason, while
`RetryAuthorizationUnavailable` requires
`target_authorization_unavailable` carrying the exact checkpoint reason
`BoundaryUnprovable`, `LinkCountUnprovable`, `UnlinkedTarget`, or
`MultipleHardLinks`, requires the first `ETXTBSY` helper to be reaped, and
forbids the second helper, fallback, and selected identity. `InspectionFailed` with `identity_changed` likewise
forbids the second helper. `SupervisionSetupFailed` is also legal in this
sequence when the second target is reaped after working-directory entry or
trace setup fails and before target handle exec; it cannot fall back. Only `ExecEacces` for a bare name,
`InterpreterAuthorizationUnavailable`, `HandleExecutionUnavailable`,
`ExecVerificationFailed`, `ExecFailed`, or `ExecStarted` proves that the second
target exec was attempted.
A second `ETXTBSY` is
`ExecFailed`. Absolute and qualified commands reject `ExecEacces` and represent
terminal `EACCES` as `ExecFailed`. Cancellation during the delay emits no
envelope and starts no child. No other errno, retry count, or delay is accepted.

Failed candidates contribute no final executable identity. Repository-owned
bare commands stop after the first statically eligible successful inspection,
record that handle as an `inspection_target`, emit `probe_not_authorized`, and
never enter pre-exec or fallback. The same stop applies to any otherwise
eligible source whose opened target is inside a repository/worktree boundary.
An earlier open or identity failure wins and no authorization failure is
appended. Exact `ENOENT`/`ENOTDIR` has precedence over `open_failed` as defined
above. Unavailable target authorization also stops without exec or fallback.
After target authorization, the observation helper runs a bounded executable
classifier over the retained handle. It requires exact ELF magic, the current
architecture's frozen machine/class/endianness tuple, `EI_VERSION` and
`e_version` equal to `EV_CURRENT`, exact ELF64 `e_ehsize = 64` and
`e_phentsize = 56`, `ET_EXEC` or `ET_DYN`, and `e_phnum` in
`1..PN_XNUM` (extended program-header counts are unsupported). v0.1 supports
only Linux `x86_64` (`EM_X86_64`, `ELFCLASS64`, little-endian) and Linux
`aarch64` (`EM_AARCH64`, `ELFCLASS64`, little-endian); another build target
fails the pre-observation capability gate. Checked arithmetic must prove
`e_phoff + e_phnum * e_phentsize` is within the retained file, and every
program header is scanned to reject `PT_INTERP`, any `PT_LOAD` with both
`PF_W` and `PF_X`, and missing, duplicate, or `PF_X` `PT_GNU_STACK`. Other header fields are
not authorization signals; a later kernel load error remains `spawn_failed`.
Exact `#!`, dynamic, writable-executable, executable-stack, or structurally
malformed ELF, wrong-machine ELF, and every non-ELF/binfmt format emit
`InterpreterAuthorizationUnavailable` before this attempt's target creation; no
header bytes, interpreter path, or raw prefix are serialized. If accepted
bytes become a script after this check,
`FD_CLOEXEC` retained-handle `execveat(AT_EMPTY_PATH)` fails before interpreter
execution; exact exec-time `ENOENT` or `ENOTDIR` is the same terminal
interpreter failure, not PATH fallback. This is intentionally conservative
because script execution or dynamic loading delegates to another executable
that this packet does not authorize. Fingerprint selection parity applies only
after successful authorization and execution of the selected eligible native
target. Interpreter, alias-ownership, or identity fail-closed branches may
reject a command that the adapter could later launch through Unix PATH
fallback; the producer must stop at that retained candidate and must not
attribute any later adapter candidate. A fixture freezes this deliberate
security divergence for both exec-time errors. Fault-injection tests freeze
these paths without a `noexec` filesystem.

All `CString` argument and environment storage and pointer arrays are built and
NUL-validated in the parent. The audited Linux pre-exec closure receives the
retained working-directory and `FD_CLOEXEC` executable descriptors. It uses
only async-signal-safe `ptrace(PTRACE_TRACEME)`, `raise(SIGSTOP)`, `fchdir`,
descriptor close, and `execveat(AT_EMPTY_PATH)`; it never allocates,
logs, locks,
resolves, or reopens a pathname after fork. It enters the exact retained
working directory, then requests tracing and
stops. Only after the parent installs
`PTRACE_O_TRACEEXEC | PTRACE_O_TRACESYSGOOD` does it close
unrelated descriptors and call `execveat`. Its stage-tagged error pipe
distinguishes working-directory-entry, trace-setup, and target
handle-exec failure. A failed `fchdir`, ptrace request, stop, or
option install produces
`supervision_setup_failed`, and the parent reaps that helper before emitting
evidence; its closed `RuntimeSupervisionSetupStage` is exactly
`WorkingDirectoryEnter` or `TraceSetup`. `TraceSetup` covers
`PTRACE_TRACEME`, the initial stop, and parent
`PTRACE_O_TRACEEXEC | PTRACE_O_TRACESYSGOOD` installation, all before target
exec.
Successful handle exec never returns. A failed target call returns its
captured errno through the distinct exec channel, so only exact target
`EACCES` reaches the fallback branch, exact `ENOENT`/`ENOTDIR` maps to terminal
interpreter authorization unavailable, exact `ENOSYS`/`EPERM`/`EINVAL` maps to
terminal `HandleExecutionUnavailable` with `Single` for the initial call or
`EtxtbsyThenCheckpointAfter150Ms` for the retry call and the fd-10 context,
and a setup errno or `ENOEXEC` cannot reach a fallback or shell execution
path.

On Windows, v0.1 independently freezes this bare-name resolver: explicit child
`PATH`, current executable directory, system directory, Windows directory,
then parent `PATH`, with only `.exe` completion. The algorithm and fixtures are
not derived from the compiler that happens to build Harness. `PATHEXT` is not
consulted. Explicit `.bat` and `.cmd` programs are `path_unusable` because the
adapter's batch handling invokes a command interpreter and violates the
no-shell boundary. Every explicit non-`.exe` extension is likewise
`path_unusable` in v0.1; supporting another extension requires a later closed
grammar revision.
The child `current_dir` is not treated as the parent search base. A qualified
relative program, relative/empty search entry, or unavailable special directory
is accepted only when its actual base is explicit and stable in the launch
context; otherwise resolution records `path_unusable`. A differential
conformance test runs the current adapter `Command` against fixed fixtures and
fails if it diverges from the frozen resolver; it never updates expected
behavior from the running compiler.

On every platform, quotes, spaces, pipes, substitutions, and redirections are
literal path characters. Resolution inspects only the configured basename,
never runs `sh`, `which`, or a package manager. Windows, absolute, and qualified
commands have one absolute selected candidate and no fallback. Unix bare-name
fallback is limited to the ordered, inspected, same-basename `EACCES` algorithm
above; it never changes arguments or executes a search helper.

The pure non-envelope Windows resolver helper carries a closed
`WindowsResolutionContextEvidence` with exactly four optional fields:
`current_executable_dir_digest`, `system_dir_digest`,
`windows_dir_digest`, and `parent_path_digest`. Each present value uses:

```text
SHA-256(domain || b"windows\0" || u64be(utf16_unit_count) || utf16le_units)
```

The domains are exactly:

```text
b"harness_runtime_windows_search_current_executable_dir_v0_1\0"
b"harness_runtime_windows_search_system_dir_v0_1\0"
b"harness_runtime_windows_search_windows_dir_v0_1\0"
b"harness_runtime_windows_search_parent_path_v0_1\0"
```

Absent is a distinct typed state with no digest; present empty hashes the
zero-unit framing. This helper value and its digests never enter a v0.1
envelope or `fingerprint_digest`. For the independent original UTF-16 units of `C:\X`
(`0043 003a 005c 0058`), the four digests in field order are exactly:

```text
4864a078702061a4fd859437dcadfce7519d755e47de020039dd4473d3651e7e
cc203ab9fd082171309ae3c4f28bae151cbc8d52e26870c25547977d196eb5ab
2fc48563c782059e0c54ca5a1c3741a991ca429434557cd070d1e87be4f7bfd7
0206b6610a84596f5fcdda5879d0fa56bb6ab45d0c88b27b25fa9d4301327db8
```

Child `PATH` retains the separate B-010 environment digest. On every platform,
effective search inputs are represented only by their frozen SHA-256 fields,
ordered attempt evidence, and resolution outcome; directory contents,
configured command, candidate paths, and raw search text are never serialized.
The supported native-binary probe child receives the exact sanitized child
`PATH` from the launch context. A shebang launcher fails before child
environment construction can select an interpreter, and the post-exec guard
prevents an accepted target from using PATH or cwd for a later image,
executable mapping, or write path to existing executable memory. Other
environment keys follow only the closed policy below.
This implements B-004, B-005, and the PATH portion of B-010.

## Handle-Based Executable Observation and TOCTOU Policy

After owner readiness and active-deadline start, each potentially blocking
filesystem operation runs in a dedicated Linux observation subprocess. This
includes cwd open/metadata, candidate `fstatat`/`openat`, final-target boundary
classification, retained-handle read/hash, exec-stop re-hash/image
classification, and both path/hash checkpoints. No such operation runs through `spawn_blocking`, a
Tokio worker, or the owner thread. The ready owner is the sole helper spawner.
Before every fork it atomically reserves the complete logical registry,
non-pidfd, one future-pidfd slot, and the role's fixed transfer slots, then creates a
private `pipe2(O_CLOEXEC)` start gate, descriptor-ready/status channel, and
single protocol/control socket. Each role has a frozen descriptor allowlist.
The common allowlist is gate-read, descriptor-ready/status, and the one
protocol/control fd. An observation role adds only the stage-required retained
cwd/target inputs; a target adds retained
cwd, retained executable, pre-exec status, and explicitly mapped stdin,
stdout, and stderr. In a target role the retained executable is mapped to
reserved child fd 10, which no other role field may occupy. The parent
precomputes a collision-free remap schedule; an already-fd-10 retained handle
keeps its original `O_CLOEXEC`, otherwise the child uses `dup3(..., 10,
O_CLOEXEC)` before closing the source descriptor.

Immediately after fork and before descriptor isolation completes, the child
transiently inherits the process-wide fd table. At most one such bootstrap
child exists per owner in addition to an already admitted target, it performs
no workload, and its inherited-reference count is not part of the numeric owner
ledger ceiling. Before waiting on the gate, the child uses only raw
async-signal-safe `dup2`/`dup3`, segmented `close_range`, `write`, `read`,
`close`, and `_exit` syscalls. It establishes the frozen stdio/allowlist
numbers, closes every inherited fd not in that role's allowlist, writes one
closed bootstrap status, then waits for one-byte `GO`. The closed
`RuntimeDescriptorBootstrapStatus` values are `DescriptorsReady`,
`DescriptorIsolationUnavailable`, and `DescriptorIsolationFailed`. This path is
allocation-free and non-panicking. Before `GO`, the child cannot access cwd or
any filesystem/proc path, invoke ptrace, inspect a target, or
exec. Inside one synchronous, non-cancellable parent critical section, the
owner forks and waits for one closed bootstrap status. Only
`DescriptorsReady` permits `pidfd_open`, registry commit of the pidfd plus
direct-child reap obligation, and the later `GO`; no client lease, response
channel, or transferred descriptor is exposed earlier. `close_range`
descriptor isolation, or an exactly
equivalent audited primitive, is part of the Linux capability contract; the
initial capability child attempts that isolation before any other role work,
and unavailability fails typed before cwd/filesystem observation, ptrace, or
exec.

This gate applies to every closed `RuntimeOwnedChildRole`:
`Observation(RuntimeObservationStage)` (including capability and retained-
handle/image helpers), `InitialTarget`, and `RetryTarget`.
The closed `RuntimeChildRegistrationStage` values are `GateCreate`, `Fork`,
`DescriptorIsolation`, `PidfdOpen`, `RegistryCommit`, and `GateRelease`.
Failure or cancellation before registry commit closes the
parent gate, so the still-gated direct child exits without workload. Until that
child is reaped, its exact positive PID cannot be reused; the owner may use only
that positive direct-child PID for bounded rollback termination/wait and never
after reap or through a negative PID or PGID. Failure after commit uses the registered
pidfd. A completed rollback returns no-envelope
`ChildRegistrationUnavailable { role, stage }`; a rollback that misses the
applicable cleanup deadline returns
`ChildRegistrationCleanupIncomplete { role, operation }`, retains the direct-
child or pidfd reap obligation and global owner permit, and exposes no lease or
descriptor. Its closed cleanup operations are `GateClose`, `Termination`, and
`Reap`. Logical slot exhaustion is instead
`OwnerResourceCapacityExceeded`; after slots are reserved, `pipe2`/`socketpair`
or `pidfd_open` resource failure is the concrete child-registration stage.
`SCM_RIGHTS` descriptor counts cannot exceed reserved transfer slots, and a
surplus is protocol-invalid descriptor mismatch. A concrete registration
failure observed before deadline has precedence over a later deadline; a
deadline error is used only when no concrete syscall failure was observed.
Cancellation returns no visible result and drives the same owner rollback. The
capability probe cannot waive this per-child ordering. The owner-side
self-pidfd preflight precedes the initial capability fork; consequently,
unsupported or policy-blocked `pidfd_open`/`pidfd_send_signal` maps to
`ContainmentUnavailable(PidfdUnavailable)`, while any per-child `pidfd_open`
failure after that successful preflight maps to
`ChildRegistrationUnavailable { role, PidfdOpen }`. A successfully registered
`Observation(CapabilityCheck)` child must exit zero, after which the owner
first observes its terminal event with
`waitid(P_PIDFD, ..., WEXITED | WNOWAIT)` and then consumes the same event
with `waitid(P_PIDFD, ..., WEXITED)`. Both results must carry the registered
child identity, `CLD_EXITED`, and status zero. `ENOSYS`, `EINVAL`, `EPERM`,
`EACCES`, `ECHILD`, or an identity/code/status mismatch returns
`ContainmentUnavailable(PidfdUnavailable)` before cwd or any later child.
Solely on this fail-closed bootstrap path, the still-unreaped direct child may
be reaped by exact positive PID while its pidfd remains held. Fallback failure
returns the existing cleanup-incomplete error and retains the owner permit.
After capability success, every registered-child wait and reap is pidfd-only.
For the initial `Observation(CapabilityCheck)` role only, `ENOSYS`, seccomp
`EPERM`/`EACCES`, or unsupported-flag
`EINVAL` from the validated descriptor-isolation syscall emits
`DescriptorIsolationUnavailable`; after the child is reaped, that status maps
only to `ContainmentUnavailable(DescriptorIsolationUnavailable)`. Any other
initial isolation error emits `DescriptorIsolationFailed` and maps to
`ChildRegistrationUnavailable { role, DescriptorIsolation }`. After capability
success, every non-ready isolation status for every role maps only to that
child-registration variant. A concrete status observed before deadline wins;
deadline is used only before any status, and cancellation emits no visible
result while the owner performs the same rollback. Descriptor-isolation and
`pidfd_open` failure are tested at every role.

After `GO`, the helper uses only preallocated
fixed-size frames, bounded buffers, raw syscalls, and an allocation-free,
non-panicking SHA-256 state; retained descriptors cross the private Unix socket
with `SCM_RIGHTS`. Protocol truncation, surplus fields, descriptor-count
mismatch, or a payload above its fixed bound fails typed.

Every observation reply is bounded by its closed stage budget.
`CapabilityCheck`, `WorkingDirectory`, `Candidate`, `TargetAuthorization`,
`SourceHash`, `PreSpawnCheckpoint`, `ExecStopCheckpoint`, and
`PostReapCheckpoint` use the one active deadline. If the applicable
deadline expires, the owner closes IPC and signals/reaps the exact helper pidfd
within the remaining cleanup path. The producer returns closed
`RuntimeFingerprintProduceError::ObservationDeadlineExceeded { stage }` when
cleanup completes, or
`ObservationCleanupIncomplete { stage, operation }` after the owner retains an
unreaped helper. Both return no envelope, so a missing required cwd identity or
candidate outcome is never fabricated. The closed stages are
`CapabilityCheck`, `WorkingDirectory`, `Candidate`, `TargetAuthorization`,
`SourceHash`, `PreSpawnCheckpoint`, `ExecStopCheckpoint`, and
`PostReapCheckpoint`; closed
operations are `Termination`, `Reap`, and `ProtocolClose`. A reply with a
truncated or oversized frame, surplus fields, descriptor-count mismatch, or an
unexpected helper exit returns
`ObservationProtocolInvalid { stage, reason }` with closed reason
`TruncatedFrame`, `OversizedFrame`, `SurplusFields`,
`DescriptorCountMismatch`, or `HelperExited`. It is not mislabeled as a
deadline. Caller cancellation follows the owner cleanup path and returns no
envelope without fabricating an observation error. These errors contain no PID,
path, or OS diagnostic.

For the initial target, the subprocess opens the selected target once and
operates on that file handle. It first uses path metadata only to skip a target
already known to be non-regular, then calls `libc::open` with
`O_RDONLY | O_CLOEXEC | O_NONBLOCK`; it never uses a potentially blocking
ordinary `File::open` on an unclassified path. Handle `fstat` is authoritative
after open and rejects a FIFO, socket, directory, device, or any race-swapped
non-regular target before reading. Symlinks are classified by the opened final
target. The retained regular handle remains nonblocking, which does not alter
regular-file reads, and is incrementally hashed in fixed-size chunks. Unix also
derives execute permission from handle mode bits. Windows extension/search
eligibility belongs to resolution and successful OS loading belongs to spawn;
the producer does not claim that an extension or parsed PE header proves
loadability. On a platform with supervised spawn, a bad-image or access failure
is `spawn_failed`; Windows v0.1 instead returns no-envelope producer error
`ContainmentUnavailable(UnsupportedPlatform)` before spawn and makes no
loadability claim.

Failure to create the retained inspection handle, except exact
`ENOENT`/`ENOTDIR`, is `identity/open_failed`. It occurs before metadata or content facts and is
mutually exclusive with `metadata_unavailable` and `read_failed`, both of which
require an opened handle. It may retain configured-command/search/attempt
digests but never a raw path, ACL, username, localized `io::Error`, handle
metadata, strong identity, executable digest, child, version, or cleanup claim.
For a bare Unix candidate this stops the search fail closed; absolute and
qualified candidates have no fallback. Exact `ENOENT`/`ENOTDIR` instead maps
to `Absent` under the command-form rules above.

The helper checks the fixed, non-caller-adjustable
`RUNTIME_FINGERPRINT_MAX_EXECUTABLE_BYTES = 67_108_864` limit against initial
metadata and again while reading, stops at byte 67,108,865, does not preallocate
the maximum, and returns only bounded typed facts. No multi-megabyte file read
occurs in the Harness process and no `std::fs::read` whole-file allocation is
allowed.

After final target authorization, the owner creates the target and remains the
sole parent-side ptrace controller and wait/reap owner. The target helper's
audited pre-exec closure is the one exception: it first
`PTRACE_TRACEME`s and stops itself before exec. The parent verifies that stop,
sets `PTRACE_O_TRACEEXEC | PTRACE_O_TRACESYSGOOD`, and resumes only into
retained-handle
`execveat(10, "", ..., AT_EMPTY_PATH)`. Exact
`EACCES`/`ETXTBSY`/`ENOEXEC` retain their
closed attempt semantics; exact `ENOENT` or `ENOTDIR` is terminal
`interpreter_authorization_unavailable` because the retained handle exists and
the closed-on-exec script/interpreter contract could not be satisfied. Exact
`ENOSYS`, `EPERM`, or `EINVAL` is terminal
`handle_execution_unavailable`; the owner reaps the helper and preserves no
selected identity or version. A
successful exec must deliver exactly one `PTRACE_EVENT_EXEC` before the new
image's first instruction. While it remains stopped and kernel executable
write denial is active, one registered observation helper re-hashes the
retained handle and opens/stats `/proc/<pid>/exe` to match the original strong
identity; no `/proc` image observation runs on the owner or async runtime.
Mismatch kills/reaps the stopped child and emits `identity_changed`; only an
exact match may resume under the post-exec syscall-stop guard below. A missing
or surplus exec event, abnormal trace state,
or inability to prove this ordering returns the closed no-envelope producer
error `ExecutionVerificationUnavailable`; the owner kills/reaps the stopped
child without resume, and no pathname fallback exists.

After that exact match, the owner uses only `PTRACE_SYSCALL` resumes and
requires alternating entry/exit stops tagged by `PTRACE_O_TRACESYSGOOD`.
`PTRACE_GET_SYSCALL_INFO` must return the expected entry or exit information
whose audit architecture exactly matches the admitted Linux `x86_64` or `aarch64` tuple;
raw register guessing is forbidden. At each entry stop, before kernel
execution, x86_64 first rejects every syscall number carrying
`__X32_SYSCALL_BIT` as no-envelope `ExecutionVerificationUnavailable`; it never
clears the bit or interprets that dispatch through the native-number table.
This check does not apply to native x86_64 numbers or aarch64. Only after that
ABI gate does the owner reject closed `RuntimeTransitiveExecutionClass`:

- `ProcessCreation`: `fork`, `vfork`, `clone`, or `clone3`;
- `ImageExecution`: `execve` or `execveat`;
- `ExecutableMapping`: the architecture's native `mmap`/`mmap2`,
  `mprotect`, or `pkey_mprotect` requesting `PROT_EXEC`, `shmat`
  requesting `SHM_EXEC`, and x86_64 `uselib`. The frozen x86_64 table
  includes `uselib`; aarch64 has no fabricated `uselib` entry;
- `ExecutableImageMutation`: `ptrace`, `process_vm_writev`, `userfaultfd`,
  `io_uring_setup`, `pidfd_getfd`, `recvmsg`, `recvmmsg`, `prctl`, every
  `personality` argument other than the side-effect-free exact
  `0xffff_ffff` query,
  `creat`, or `open`/`openat`/`open_by_handle_at`/`openat2` whose flags request
  `O_WRONLY`, `O_RDWR`, or `O_TRUNC`; and
- `ProcessSignalling`: `kill`, `tkill`, `tgkill`, `rt_sigqueueinfo`,
  `rt_tgsigqueueinfo`, or `pidfd_send_signal`, for every target, signal, PID,
  TID, pidfd, zero, negative, process-group, and broadcast form.

The fixed-size `open_how` prefix used by `openat2` is copied from the stopped
single-threaded target with one bounded `process_vm_readv`; an unreadable
pointer, a size smaller than the flags field, nonzero unknown tail bytes, or
unknown/conflicting flag bits is `ExecutionVerificationUnavailable`. It is
never resumed as a read-only open. Descriptor isolation proves that the target
inherits no writable memory fd or Unix socket. Denying every later
write-capable open blocks `/proc/self/mem`, `/proc/thread-self/mem`, numeric
proc aliases, and mount aliases without parsing an attacker-controlled
pathname. Denying `recvmsg`/`recvmmsg` and `pidfd_getfd` prevents later
`SCM_RIGHTS` or remote-fd acquisition; denying `prctl` prevents the target from
loosening external ptrace access. `io_uring_setup` is denied so asynchronous
open/write cannot bypass syscall-entry classification. The classifier requires
exactly one non-executable `PT_GNU_STACK` and rejects W+X `PT_LOAD`; denying
`personality` changes prevents `READ_IMPLIES_EXEC` from converting a
`PROT_READ` mmap/mprotect request into an executable mapping.

The closed trace state begins `AwaitInitialExecExit`: after the already verified
`PTRACE_EVENT_EXEC`, exactly one tagged syscall-exit stop for that authorized
`execveat` is accepted before `AwaitEntry`. Thereafter an allowed entry moves to
`AwaitExit`, its matching tagged exit returns to `AwaitEntry`, and only `exit`
or `exit_group` may terminate directly from their allowed entry without a
matching exit stop. A validated signal-delivery stop is legal only in
`AwaitEntry` and never advances syscall state. The owner distinguishes it
from tagged syscall, ptrace-event, seccomp, and group stops using the full wait
status plus `PTRACE_GETSIGINFO`, then resumes with `PTRACE_SYSCALL` while
injecting the same signal. A caught or ignored signal keeps `AwaitEntry`; a
terminating signal yields only `terminated_by_signal` after complete capture.
`SIGKILL` may terminate directly with the same semantic result. A delivery
stop in `AwaitInitialExecExit` or `AwaitExit`, malformed siginfo, a group,
seccomp, event, untagged, wrong-op, wrong-architecture, or duplicate stop
returns `ExecutionVerificationUnavailable` and starts cleanup. The active
deadline covers every trace stop and resume through target exit.

The owner never resumes a denied entry. It kills/reaps the stopped target and
records `version_probe/transitive_execution_denied` with only the closed class,
no syscall number, argument, path, or OS text, and no version. If termination
or reap does not complete, the normative lifecycle-cleanup failure is appended
and the owner retains the stopped obligation. Missing,
surplus, untagged, out-of-order, or unreadable syscall stops return no-envelope
`ExecutionVerificationUnavailable` and run the same cleanup. This guard means
the admitted initial current-architecture static ELF and kernel-provided
immutable vDSO are the only executable mappings allowed to run, and target
code cannot gain a write path to either. PATH/cwd helper launches, dynamic
loading, JIT mappings, writable executable segments, executable stacks,
asynchronous syscall submission, processes, and threads are deliberately
unsupported in v0.1. Target-initiated process signalling is also unsupported.
A validated delivered signal is exit/control-flow evidence, never
`ProcessSignalling`; cleanup-originated signals preserve the earlier failure
and cannot become semantic exit evidence.

The retained strong identity is device/inode from handle metadata on Unix and
volume serial plus 128-bit `FILE_ID_INFO` from the opened handle on Windows.
Path, mtime, extension, or a weaker optional metadata field is not a fallback.
If the opened handle cannot provide the specified strong identity, the producer
records `identity/metadata_unavailable` before version attribution. Before the
pre-spawn checkpoint, platform handle APIs must also return the final target
path used by the B-007 boundary classifier. Failure to prove that target lies
outside every validated repository/worktree root records
`target_authorization_unavailable` or `probe_not_authorized` and stops without
spawn. Initial authorization, pre-spawn, the post-`ETXTBSY` retry gate,
exec-stop, and post-reap checkpoints all re-read `st_nlink`. Before target
creation, zero uses
`target_authorization_unavailable/UnlinkedTarget`, a value greater than one
uses `target_authorization_unavailable/MultipleHardLinks`, and an unavailable
value uses `target_authorization_unavailable/LinkCountUnprovable`. During retry
each produces `RetryAuthorizationUnavailable` with the same matching reason
after the first `ETXTBSY` helper is reaped and before a second helper exists.
At exec-stop, a changed observed count produces
`identity_changed`/`ExecVerificationFailed` and kills before resume; inability
to observe the count returns no-envelope `ExecutionVerificationUnavailable`
and also kills/reaps without resume. After resume or reap, a changed count
produces `identity_changed`, while post-reap observation unavailability
produces `identity/metadata_unavailable`; both discard version evidence.
`path_unusable` remains exclusive to resolution. Immediately
before spawn and again after the child is reaped, one blocking checkpoint
re-reads and re-hashes the retained executable handle and reopens the private
candidate reference to compare strong identity. On Unix both checkpoint reopens use the
same `O_RDONLY | O_CLOEXEC | O_NONBLOCK` flags and authoritative handle
classification as the initial open. Absolute references use `open`;
working-directory-relative references use `openat` against the same retained
directory descriptor used for initial observation and child `fchdir`.
Replacing the configured cwd pathname therefore cannot redirect resolution or
either checkpoint; a race-swapped FIFO or other special file in the retained
directory cannot block and becomes `identity_changed`.
Initial, pre-spawn, exec-stop, and post-reap retained-handle size/digest
observations must match, and both later path identities must equal the retained
handle. A symlink is identified
by the opened target, not by mixing link metadata with target bytes.

If any size, digest, or strong-identity comparison fails, the envelope records
`identity/identity_changed`, emits no version fact, and does not associate the
version output with the inspected executable digest. A pre-spawn mismatch
prevents the probe; a post-reap mismatch discards the candidate version. The
successful source correlation remains named `checkpoint_consistent_path`;
mutation and restoration entirely between path checkpoints remains a
pathname-history gap. The separate `exec_stop_consistent_handle` fact requires
the verified pre-first-instruction exec stop, stopped-image strong identity,
and matching retained-handle digest while kernel write denial is active. It
proves no changed target or interpreter instruction ran before validation and
that the resumed executable bytes equal the recorded digest. This implements
B-006 and its non-goal without promoting path history to attestation.
