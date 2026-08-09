# Runtime Product Contract

## Linked Spec Packet

This document is the runtime-specific product contract for GH-1733. Read it
with `product.md`, `runtime-observation.md`, `runtime-supervision.md`,
`tech.md`, and `tasks.md`.

## Runtime User-Visible Behavior

4. **B-004:** Runtime resolution treats the configured executable as exactly
   one command name or path and never invokes a shell or parses embedded
   arguments, quoting, pipes, substitutions, or redirections. Pure Windows
   resolver helpers mirror the frozen Windows search behavior below,
   independent of the compiler used to build Harness; Unix envelope production
   uses an explicit safe subset that preserves
   `EACCES` fallback but deliberately rejects `ENOEXEC` shell fallback. On
   Unix, a qualified relative path and
   relative or empty `PATH` entries are resolved from the declared child
   working directory. Pure command-form helpers model the closed set
   `unix_bare`, `unix_absolute`, `unix_qualified`, `windows_bare`,
   `windows_absolute`, or `windows_qualified`. A v0.1 envelope payload records
   only one of the three Unix forms; every Windows form is helper-only and is
   rejected by envelope construction and strict parsing. The admitted Unix
   form distinguishes search skips from failures of a configured path.

   Every admitted Unix v0.1 payload includes
   `configured_command_digest = SHA-256("harness_runtime_configured_command_v0_1\0" || platform_tag || unit_count_be || exact_units)`.
   `platform_tag` is exactly `b"unix\0"` or `b"windows\0"`;
   `unit_count_be` is an unsigned fixed-width `u64` in big-endian order.
   Unix exact units are the raw `OsStr` bytes. The pure cross-platform digest
   helper also freezes Windows exact units as the original UTF-16 code units
   serialized little-endian, with a count in UTF-16 units rather than bytes;
   that Windows digest never enters a v0.1 envelope. There is no UTF-8,
   case, separator, dot-segment, symlink, or absolute-path normalization, and
   the raw command is never serialized. The configured child working-directory
   spelling is hashed independently with domain
   `"harness_runtime_working_directory_v0_1\0"` and the same platform tag,
   count, and exact-unit framing. On Unix, after isolation and sandbox
   validation but before executable resolution, the producer opens that
   directory once, requires directory handle metadata, retains the handle for
   child `fchdir`, and records
   `working_directory_identity_digest =
   SHA-256("harness_runtime_working_directory_identity_v0_1\0" ||
   u64be(device) || u64be(inode))`. Open or metadata failure is typed
   `working_directory_unavailable` and emits no fingerprint; the raw path,
   device, and inode are never serialized.

   Every Unix candidate is a private closed reference: `absolute(path)` or
   `working_directory_relative(path)`. Qualified relative commands and
   relative or empty `PATH` entries produce the latter. Their preliminary
   observation, authoritative open, and both later checkpoint reopens use
   `fstatat`/`openat` (or exact handle-relative equivalents) against the same
   retained working-directory handle; they never reconstruct and access an
   absolute pathname. For evidence only, the candidate digest uses the lexical
   candidate spelling joined to the configured working-directory spelling;
   that string is never used for access, and the separate directory-identity
   digest binds which retained directory supplied the target.

   The pure Windows resolver helper makes a bare name follow the frozen v0.1 search
   order and `.exe` completion rules using explicit launch-context inputs; it
   does not use `PATHEXT`, and `.bat`/`.cmd` are `path_unusable` because the
   adapter's batch handling would invoke a command interpreter contrary to the
   no-shell boundary. Every
   explicitly named non-`.exe` extension is `path_unusable` in v0.1. A
   qualified relative path or relative/empty search input whose base cannot be
   proven is also `path_unusable`.

   The pure non-envelope Windows resolution helper carries exactly four
   optional context fields:
   `current_executable_dir_digest`, `system_dir_digest`,
   `windows_dir_digest`, and `parent_path_digest`. Each present field is
   `SHA-256(domain || b"windows\0" || u64be(utf16_unit_count) ||
   utf16le_units)` under its field-specific
   `harness_runtime_windows_search_<field>_v0_1\0` domain frozen in the tech
   spec. Absent is distinct from present empty. Neither these fields nor their
   raw directories and parent PATH enter a v0.1 envelope or fingerprint
   payload.

   For a platform admitted by the static matrix, after isolation, sandbox, and
   public output-limit validation and before any digest, PATH split, lexical
   join, owner reservation, cwd open, helper, or child, v0.1 checks each launch
   input through at most limit-plus-one exact OS units. The configured command,
   configured working-directory spelling, exclusion-surviving sanitized child
   `PATH` and policy-selected `CLAUDE_CONFIG_DIR`, and each present Windows
   current-executable directory, system directory, Windows directory, and
   parent `PATH` have the inclusive per-field limit 65,536. Unix units are
   exact `OsStr` bytes; Windows units are original UTF-16 code units.
   Observation environment entries and setup-secret names are each limited to
   1,024, and every environment key or setup-secret name is limited to 1,024
   exact OS units before canonicalization or value access. These validated
   fields mathematically bound every reached derived lexical candidate to at
   most 196,610 units:
   65,536 cwd units + separator + 65,536 relative PATH-entry units + separator
   + 65,536 command units. There is no separate, caller-reachable
   derived-candidate limit failure. The closed limit reasons are `configured_command`,
   `working_directory`, `windows_current_executable_directory`,
   `windows_system_directory`, `windows_directory`, `windows_parent_path`,
   `observation_environment_entries`, `environment_key`,
   `setup_secret_names`, `setup_secret_name`, `child_path`,
   and `claude_config_directory`.
   Limit-plus-one returns typed no-envelope `launch_input_limit_exceeded`
   without a digest, split, join, owner, fd, observation, child, truncation, or
   fallback. Exact precedence is isolation, sandbox, actual unsupported
   platform, public output range, command/cwd/explicit search-base limits,
   environment count/key limits, setup-secret count/name limits, key
   shape/canonicalization/collision, setup-secret exclusion, selected
   PATH/Claude value limits, empty/NUL/shape validation, digest, then owner
   admission. The already byte-bounded Unix `PATH` is traversed lazily after
   admission; it is not rejected merely because it contains more than 64
   entries. Each reached candidate join uses checked arithmetic and allocates
   no more than the already-proven 196,610-unit maximum.
   Cross-platform digest/model helpers enforce the same relevant limits before
   hashing.
   Over-limit rejection is a documented representability divergence from an
   adapter launch, not evidence for another PATH candidate.

   Unix bare-name execution uses a frozen Harness search algorithm rather than
   delegating to `execvp`: candidates keep the exact basename and sanitized
   `PATH` order and at most 64 entries are observed. Missing, non-regular, or
   mode-ineligible candidates are skipped without execution only for
   `unix_bare`. For `unix_absolute` or `unix_qualified`, an existing
   non-regular or mode-ineligible candidate is terminal
   `identity/not_regular_file` or `identity/not_executable`. Exact `ENOENT` or
   `ENOTDIR` from either preliminary observation or the authoritative open is
   `absent`: a bare search continues, while an absolute or qualified command
   ends as `path_not_found`. Every other open failure is
   `identity/open_failed` and stops. Reaching
   entry 65 before a terminal outcome is
   `path_resolution/candidate_limit_exceeded`; an earlier terminal outcome
   succeeds or fails normally without inspecting or counting the remaining
   entries. After inspection and the pre-spawn
   checkpoint, a bounded parser first requires a current-architecture static
   ELF. Linux `x86_64` accepts only `EM_X86_64`/ELF64/little-endian and Linux
   `aarch64` only `EM_AARCH64`/ELF64/little-endian. Both require exact ELF
   magic, `EI_VERSION` and `e_version` `EV_CURRENT`, ELF64 header size 64,
   program-header entry size 56, `ET_EXEC` or `ET_DYN`, a nonzero
   non-extended program-header count below `PN_XNUM`, overflow-safe in-file
   program-header bounds, no `PT_INTERP`, no `PT_LOAD` carrying both `PF_W`
   and `PF_X`, and exactly one non-executable `PT_GNU_STACK`; other build targets fail the
   capability gate before observation. Scripts, dynamic, writable-executable,
   executable-stack, or structurally malformed ELF, wrong-machine ELF, and
   every other format fail
   `interpreter_authorization_unavailable` before target creation so neither an
   ELF loader nor `binfmt_misc` interpreter can run. Linux then keeps
   `FD_CLOEXEC` on the retained authorized handle, maps it collision-safely to
   child fd 10, and uses direct no-shell
   `execveat(10, "", ..., AT_EMPTY_PATH)` under a traced
   pre-first-instruction exec-stop. `FD_CLOEXEC` makes interpreter-script
   `execveat` fail before the interpreter can run. A successful native exec
   must stop at `PTRACE_EVENT_EXEC`; while the kernel's executable write denial
   is active and before the new image executes an instruction, a registered
   observation helper re-hashes the retained handle and verifies the stopped
   image's strong identity through `/proc/<pid>/exe`. Only an exact match
   resumes. The call's `argv[0]` is the exact
   original configured command `OsStr` spelling used by the adapter; resolution
   never substitutes the candidate spelling. Linux nevertheless exposes exact
   `AT_EXECFN = "/dev/fd/10"` for this `AT_EMPTY_PATH` launch, and fd 10 is
   closed in the new image. That deliberate difference from the adapter's
   pathname launch is represented on every direct-exec attempt by the sole
   closed execution context
   `linux_fd_cloexec_execveat_empty_path_fd_10`, which enters the fingerprint.
   A static executable that derives behavior or resources from `AT_EXECFN` may
   therefore produce different output or fail; the producer never claims
   pathname-launch parity for that program. No path-based `execve` is
   permitted after authorization. The pre-observation gate separately requires
   the ptrace exec-stop and post-exec syscall guard. If those are unavailable,
   the producer returns no-envelope
   `containment_unavailable/post_exec_guard_unavailable`. If those mandatory
   containment capabilities are present, the fixed target helper makes the
   actual `execveat(10, "", ..., AT_EMPTY_PATH)` call. Exact `ENOSYS`, `EPERM`,
   or `EINVAL` from that fully frozen call is terminal
   `handle_execution_unavailable`: every created target helper is reaped, no
   target instruction ran, and path execution is not a fallback. Exact
   `EACCES` retains the
   separate bare-name fallback rule. Only that exact target-handle `EACCES` may
   discard that bare-name candidate and continue to the next same-basename
   entry. An exact first `ETXTBSY` waits 150 milliseconds, repeats target
   authorization plus the full retained-handle/path identity checkpoint, and
   retries that same retained candidate handle exactly once.
   Identity change prevents retry; a second `ETXTBSY` is terminal
   `spawn_failed`. Exact retained-handle exec-time `ENOENT` or `ENOTDIR` is terminal
   `interpreter_authorization_unavailable`; `ENOEXEC` is terminal
   `spawn_failed` and never invokes `/bin/sh`; every other spawn error is
   terminal. Absolute and qualified
   commands may perform the one same-candidate `ETXTBSY` retry but never search
   or fallback to another candidate. Ordered attempt evidence retains only
   domain-separated candidate
   path digests and the closed outcomes defined by B-008. Candidate digests use
   `"harness_runtime_resolution_candidate_v0_1\0"` followed by the same exact
   platform tag, `u64` big-endian unit count, and OS units as the configured
   command digest. It also records the closed exec sequence `none`, `single`,
   or `etxtbsy_then_checkpoint_after_150_ms`. `none` requires absent execution
   context; the other two require exactly
   `linux_fd_cloexec_execveat_empty_path_fd_10`. The checkpoint sequence is legal
   only when the first direct exec returned exact `ETXTBSY` and does not imply
   that authorization allowed a second exec. The first successful exec is the
   selected executable;
   exhausting only `EACCES` candidates is `bare_eacces_exhausted`, while no
   inspectable candidate is `path_not_found`. Resolution never invokes
   `which`, a package manager, or an arbitrary candidate command.
   Fingerprint selection parity applies only after the selected eligible
   native target is successfully authorized and executed. Interpreter,
   alias-ownership, and identity fail-closed branches intentionally may reject
   a command that the adapter could later launch through Unix PATH fallback.
   In particular, retained-handle `ENOENT`/`ENOTDIR` stops at that candidate
   and never attributes or executes a later same-basename candidate. All
   child-creation and no-child statements are attempt-local: a terminal
   pre-target result on a later candidate preserves every earlier ordered
   attempt, including a fully reaped `exec_eacces` target, while proving that
   the terminal candidate created no target. Each direct-exec target is fully
   reaped and removed from the exact pidfd registry before search proceeds to
   another candidate. A later terminal classifier or candidate-limit failure
   therefore publishes only after the registry is empty; incomplete reap
   retains exact ownership and never rewrites the terminal attempt.

   A bare Unix command with sanitized child `PATH = Unset` is
   `path_unusable` before candidate observation. v0.1 never guesses libc's
   platform-dependent default search path; absolute and qualified commands
   remain valid with PATH unset. Repository-owned bare commands stop after the
   first statically eligible,
   successfully inspected candidate, record its identity as
   `inspection_target` plus `probe_not_authorized`, and perform no exec attempt
   or fallback. The same stop applies when any otherwise eligible source
   resolves to an opened target inside a validated repository/worktree
   boundary. That target is never called selected or executed. An earlier
   open/identity failure remains the sole earliest failure.
5. **B-005:** The version child receives only the sanitized `PATH` value
   included in the B-004 launch context. No caller can declare or expose
   another key. Because script execution delegates to an interpreter whose own
   target and transitive launch behavior are not bound by the inspected file
   identity, v0.1 accepts only a current-architecture static ELF with no
   `PT_INTERP` or W+X `PT_LOAD` and with exactly one non-executable
   `PT_GNU_STACK`. Exact leading
   `#!`, dynamic/writable-executable/executable-stack/malformed ELF, and any
   non-ELF format fail closed with
   `version_probe/interpreter_authorization_unavailable` after target
   authorization but before this attempt's target creation. It never invokes a
   interpreter or loader and never searches child `PATH` for one. If accepted
   static bytes change into a script after that check,
   `FD_CLOEXEC` makes `execveat(AT_EMPTY_PATH)` fail before interpreter
   execution; exact exec-time `ENOENT`/`ENOTDIR` is mapped to the same terminal
   interpreter failure. A later revision may support scripts only by resolving,
   retaining, fingerprinting, and authorizing the complete interpreter chain.
   Evidence classification comes from the closed B-010 policy, never from a
   caller string, and every non-`PATH` v0.1 policy key is excluded from the
   version child. Platform search inputs outside child `PATH` remain explicit
   resolution context and are never misrepresented as child environment.
   Every name in `codex.cloud.setup_secret_env` is excluded from both the child
   environment and persisted fingerprint facts before policy matching,
   regardless of spelling. Sensitivity is not guessed from substrings such as
   `TOKEN` or `SECRET`; no setup-only or undeclared value is inserted into probe
   `envp`, read by the producer for evidence, or persisted. This is not a claim
   that an already authorized executable cannot actively read same-UID host
   state outside its supplied environment. The producer counts and bounds
   environment/setup names before canonicalization, but never reads, copies,
   bounds, or hashes an undeclared or setup-secret-excluded value. Only
   exclusion-surviving selected `PATH` and `CLAUDE_CONFIG_DIR` values receive
   the 65,536-unit value check. An excluded over-limit Claude directory is
   absent without a limit error; an excluded over-limit PATH becomes `Unset`,
   and a later bare Unix lookup returns `path_unusable`.
   For an accepted static target, sanitized PATH remains evidence and the sole
   child environment value, but the B-007 post-exec guard prevents it or the
   retained working directory from selecting any later process image, new
   executable mapping, or write path to existing executable memory.
6. **B-006:** Executable identity comes from one opened regular-file handle,
   not separate path-based metadata and content reads. Size and SHA-256 cover
   the bytes read from that handle. Unix first rejects a path already known to
   be non-regular, then opens the remaining candidate with nonblocking,
   close-on-exec semantics and treats handle metadata as authoritative before
   any read. A FIFO, socket, directory, or device cannot block the producer
   while waiting for ordinary read access and never reaches hashing. Unix
   executable permission is derived from handle metadata. Supported Linux also
   requires authoritative handle `st_nlink == 1` before target authorization.
   An unavailable count is
   `target_authorization_unavailable/link_count_unprovable`; zero is
   `target_authorization_unavailable/unlinked_target`; a count greater than one
   is `target_authorization_unavailable/multiple_hard_links`. Each
   forbids the next exec or fallback. This intentionally rejects legitimate
   multiply linked binaries
   because v0.1 cannot prove every hard-link alias lies outside repository
   boundaries; it does not claim to eliminate bind-mount or namespace aliases.
   Windows candidate
   eligibility comes from the B-004
   filename/search contract, while actual loadability can be proven only by a
   successful supervised spawn. On a platform where process supervision is
   available, a bad image or access error is `spawn_failed`, not a fabricated
   `not_executable`; Windows v0.1 returns typed producer error
   `containment_unavailable` with no envelope before that attempt. The producer
   enforces the fixed, non-caller-adjustable
   `RUNTIME_FINGERPRINT_MAX_EXECUTABLE_BYTES = 67_108_864` byte ceiling before
   and during reading, and does not allocate the declared maximum eagerly.
   Every potentially blocking cwd open, candidate open/stat, boundary
   classification, content read/hash, and path checkpoint runs in a dedicated
   Linux observation subprocess, never in `spawn_blocking`, a Tokio worker, or
   the runtime-independent owner thread. The ready owner synchronously creates
   each subprocess, acquires its pidfd, and records reap ownership before it
   exposes any client lease, descriptor, or response channel. That
   create/register critical section contains no await, cancellation point, or
   fallible ownership transfer; if the client disappears later, the owner
   already has cleanup authority. The subprocess uses a fixed, allocation-free
   syscall protocol and transfers only bounded facts and retained descriptors
   over a private socket. The one active deadline begins before the first cwd
   observation and bounds each wait; expiry closes IPC, requests termination
   through that pidfd, transfers sole ownership to the owner, and returns a
   closed typed producer error with no envelope, without waiting synchronously
   for potentially uninterruptible filesystem I/O. The error is
   `observation_deadline_exceeded` when cleanup completes and
   `observation_cleanup_incomplete` with a closed stage/operation when it does
   not. A helper that the kernel has not yet reaped
   remains explicitly owned and cannot hold the producer future or an
   in-process worker indefinitely; no evidence claims it was terminated.
   Platforms without this kill-isolated, strongly identified helper protocol
   return typed producer error `containment_unavailable` with no envelope before
   cwd or executable observation.
   Exact leading `#!` on the retained authorized handle is terminal
   `interpreter_authorization_unavailable`. The same bounded check rejects any
   `PT_INTERP`, non-ELF, wrong-architecture, or malformed image. The retained
   handle is re-read and
   re-hashed immediately before spawn, at the successful
   `PTRACE_EVENT_EXEC` stop while kernel write denial is active, and after the
   child is reaped, within that same active deadline; all four observations
   and link counts must match before version attribution. The initial,
   pre-spawn, and post-`ETXTBSY` authorization gates require link count one.
   A changed count at exec-stop kills before resume with
   `identity_changed/exec_verification_failed`; a later change discards version
   with `identity_changed`. If link-count observation itself is unavailable at
   exec-stop, the producer kills/reaps without resume and returns
   `execution_verification_unavailable` with no envelope; if unavailable only
   after reap, version is discarded with `identity/metadata_unavailable`. At both later path
   checkpoints the private candidate reference is reopened with the same Unix
   `O_RDONLY | O_CLOEXEC | O_NONBLOCK` classification as the initial open:
   absolute references use `open`, and working-directory-relative references
   use `openat` against the retained directory handle,
   rejects a race-swapped non-regular target from handle metadata without
   blocking, and must still identify the retained handle using
   device/inode on Unix or volume serial plus 128-bit file ID on Windows. If
   strong identity is unavailable, observation fails typed rather than falling
   back to path, timestamps, extension, or a parsed PE header. Replacement,
   in-place content change, or inability to correlate version output with the
   inspected checkpoints emits `identity_changed` and discards version
   evidence. The path correlation remains named
   `checkpoint_consistent_path`: mutation and restoration entirely between
   path checkpoints remains a residual pathname-history gap. Execution
   attribution is separately named `exec_stop_consistent_handle`: successful
   `PTRACE_EVENT_EXEC`, stopped-image strong identity, and retained-handle hash
   under kernel write denial prove that no changed target or interpreter
   instruction ran before validation and that the resumed executable bytes
   equal the recorded digest. Resume occurs only under the B-007 syscall-stop
   guard: every later process-creation or image-execution syscall, every
   request for a new executable mapping, and every closed existing-image
   mutation or native kernel-code-loading syscall is stopped before kernel
   execution and fails closed. Thus
   an allowed static target cannot use PATH, cwd, `dlopen`, `/proc/self/mem`,
   asynchronous I/O submission, or a child process to execute
   repository/worktree code.
7. **B-007:** A version probe has one lifecycle covering authorization, target
   creation, exact-pidfd supervision, bounded output capture, wait/reap,
   post-reap identity verification, and cleanup. Stdout and stderr are read
   incrementally under one inclusive combined limit. The validated public
   value is in `1..=65_536`; an invalid value fails before allocation or
   process creation, exactly `max_output_bytes` is allowed, and observing one
   additional byte records `output_limit_exceeded`.

   On supported Linux, after the isolation and sandbox gates but before cwd or
   executable observation, the producer atomically acquires one of exactly
   eight process-global owner permits. Capacity exhaustion returns no-envelope
   `containment_unavailable/owner_capacity_exhausted` without waiting,
   opening an fd, or starting a thread. The runtime-independent owner must
   complete its fixed 1,000 ms readiness handshake. Start, ready-timeout, and
   stop/join failures use the closed reasons `owner_start_failed`,
   `owner_ready_timeout`, and `owner_stop_join_timeout`. Cancellation uses
   the same bounded stop/join path. A created owner retains its permit until
   its thread exits and its exact pidfd registry is empty; API return, timeout,
   cancellation, or cleanup-incomplete cannot release it early.

   Each fork occurs with every blockable signal blocked after saving the owner
   thread's exact mask. The parent restores that mask immediately; restore
   failure closes the gate, rolls back the child, and returns
   `containment_unavailable/signal_isolation_unavailable`. The child resets all
   catchable dispositions while blocked, then installs the exact empty mask
   before `DescriptorsReady`; every target exec therefore inherits default
   dispositions and no blocked signals.

   Each ready owner has two pidfd slots, one for the current target and one for
   the current observation or capability helper. Across eight owners the exact
   maximum is 16 pidfds. Its non-pidfd ledger has 28 slots. Before
   `DescriptorsReady`, at most one bootstrap child per owner may transiently
   inherit the process-wide descriptor table in addition to an admitted
   target. It performs no workload and has no numeric owner-ledger ceiling. The
   active deadline bounds waiting for `DescriptorsReady`; expiry starts exact
   direct-child rollback under the cleanup deadline, and no release is claimed
   before reap. Cleanup-incomplete retains the obligation and permit. After
   readiness one child has at most 12 allowlisted references. After target
   exec, the target retains exactly three stdio references; a concurrent
   exec-stop observation helper retains at most five, for eight simultaneous
   child references. No other phase permits two live child roles. Thus the
   post-ready retained ceiling is 28 + 12 = 40 per fingerprint and 320
   globally. There is no anchor, membership helper,
   membership batch, PGID ledger, or transferred member-pidfd capacity.
   Logical slots are reserved before fd creation, fork, `pidfd_open`, or
   `SCM_RIGHTS`; capacity failure is typed no-envelope and the owner retains
   all prior obligations.

   The owner performs a self-pidfd open/signal preflight before the capability
   helper and before cwd access. The successful capability child then proves
   wait and reap by one validating
   `waitid(P_PIDFD, WEXITED | WNOWAIT)` followed by a consuming
   `waitid(P_PIDFD, WEXITED)`; both results require its registered identity,
   `CLD_EXITED`, and status zero. A completed failure path returns
   `containment_unavailable/pidfd_unavailable` before cwd or later children.
   Solely for this bootstrap, a `WNOWAIT` failure/mismatch or consuming-call
   error while the child remains unreaped permits exact positive-PID reap while
   the pidfd remains held. A successful consuming wait has reaped the child, so
   a malformed identity/code/status result closes and unregisters it without a
   positive-PID operation. Fallback failure returns the existing
   cleanup-incomplete error and retains the obligation and owner permit. After
   success every registered-child wait/reap is pidfd-only. The owner is the
   sole process creator, parent-side ptrace
   controller, helper spawner, waiter, and reaper; the target pre-exec
   `PTRACE_TRACEME` is the only audited exception. Every capability helper,
   observation helper, initial target, and retry target is created behind a
   close-on-exec start gate. Before `GO`, the child performs only
   allocation-free descriptor isolation and reports one closed bootstrap
   status. The owner must reserve the role, open its pidfd, and atomically
   register the exact pidfd plus reap obligation before releasing `GO` or
   exposing a cancellable lease. Registration failure closes the gate and may
   use only the exact still-unreaped direct-child positive PID for bounded
   rollback. After registration, all signalling uses the registered pidfd
   identity. Wait/reap is pidfd-only after capability success, except for the
   failed initial capability bootstrap's exact positive-PID reap while its
   pidfd remains held. No negative PID, PGID, post-reap PID, or `/proc`
   membership scan is permitted.

   A single five-second active deadline begins after owner readiness and before
   cwd observation. It covers capability and observation helpers, retained cwd
   and executable observation, authorization, static-ELF classification,
   ptrace setup and exec-stop verification, initial or retry target execution,
   the optional 150 ms `ETXTBSY` delay, bounded output collection, exact
   target exit/reap, and the post-reap identity checkpoint. Observation
   deadline, protocol, or cleanup failure is a typed no-envelope producer
   error. Expiry before verified resume kills and reaps the registered stopped
   target without resume and returns
   `execution_verification_unavailable`; only expiry after verified resume
   records `version_probe/timeout`. Cleanup has a separate five-second
   deadline.

   Supported Linux requires `close_range` descriptor isolation,
   `pidfd_open`, `pidfd_send_signal`, parent-child ptrace with
   `PTRACE_O_TRACEEXEC | PTRACE_O_TRACESYSGOOD`, exact
   `PTRACE_GET_SYSCALL_INFO`, and strong stopped-image identity before cwd
   observation. Other platforms fail no-envelope before cwd access. The
   capability helper is owner-traced with `PTRACE_SEIZE`; it never calls
   `PTRACE_TRACEME`. Exact `ENOSYS`, `EPERM`, or `EINVAL` from an
   eligible target's frozen fd-10 `execveat(AT_EMPTY_PATH)` is candidate-local
   `handle_execution_unavailable`, never a capability error.

   After the verified `PTRACE_EVENT_EXEC`, the owner resumes only with
   `PTRACE_SYSCALL` and classifies every syscall-entry stop before execution.
   x86_64 x32 dispatch is rejected before native decoding. Denied classes are
   process creation; image execution; executable mapping, including
   x86_64 `uselib`; executable-image mutation, including every `openat2`;
   native kernel-code loading through `bpf`, `init_module`, or `finit_module`;
   and process signalling.
   `mmap`/native `mmap2`, `mprotect`, `pkey_mprotect`, or `shmat`
   requesting executable access are executable mapping. The frozen native
   x86_64 table additionally classifies `uselib` as executable mapping;
   aarch64 has no fabricated `uselib` entry. A denied syscall never executes,
   produces `transitive_execution_denied`, and starts registered-pidfd
   cleanup. A validated signal-delivery stop in `AwaitEntry` is reinjected
   with `PTRACE_SYSCALL`: caught or ignored signals continue, while complete
   capture of a fatal signal yields only `terminated_by_signal`. Delivery in
   another state, malformed siginfo, group, seccomp, event, missing, surplus,
   untagged, out-of-order, or unreadable stops return no-envelope
   `execution_verification_unavailable`. Direct `SIGKILL` death from
   `AwaitEntry` or `AwaitExit` yields `terminated_by_signal`; from
   `AwaitInitialExecExit` it is `execution_verification_unavailable`.

   The only setup stages after the capability gate are
   `working_directory_enter` and `trace_setup`. Failure on the initial or
   post-`ETXTBSY` target is `supervision_setup_failed`, the registered target
   is reaped, handle exec is not attempted, and the errno is never PATH
   fallback. There are no `anchor_setup` or `group_join` stages.

   Success requires all of the following on the same attempt: the verified
   target executed; the exact registered target exited and was reaped; both
   output streams were captured completely within the bound; the post-reap
   retained-handle/image identity checkpoint passed; and the owner's exact
   pidfd registry is empty. Cleanup signals and reaps only registered target or
   helper pidfds, then closes or drains output within the remaining cleanup
   deadline. An incomplete cleanup records the closed lifecycle failure where
   envelope-capable, otherwise returns the typed no-envelope observation error,
   while the owner keeps its permit and exact obligations.

   The evidence claim is deliberately limited: the registry is empty and no
   denied process-creation syscall was executed while the target was guarded.
   It does not claim descendant-tree emptiness, process-group containment, or
   knowledge of unregistered processes. Descriptor isolation plus the
   pre-execution syscall guard prevents the admitted target from creating a
   descendant or passing an output descriptor to another process. A process
   stuck in uninterruptible I/O may outlive both deadlines, so no result
   fabricates termination. Caller cancellation activates the same pre-existing
   owner and emits no fingerprint. Root-only `kill_on_drop` and detached
   `ManagedChild` reaping are not completion evidence.

   An unrelated same-session process may repeatedly call `setpgid` before,
   during, and after the probe. Because the owner did not create or register
   it, that PID never enters evidence, is never inspected as membership, is
   never signalled, and cannot change the exact-registry success result.

8. **B-008:** Probe incompleteness is represented by closed typed phase and
   kind values, not caller-defined strings. The v0.1 vocabulary is the table
   below. A failure record contains bounded, redacted structured facts only,
   such as an exit code or limit; localized OS messages, raw paths, output, and
   environment values are not part of the canonical digest. Failures serialize
   in deterministic phase/kind order, and an unsupported phase/kind is rejected.
9. **B-009:** Version success requires a stable executable identity, exit code
   zero, bounded valid UTF-8 output, and one exact whole-stream match for the
   selected B-002 runtime grammar. `VERSION` is strict ASCII SemVer with exactly
   three numeric core components, no invalid leading zero, and optional
   prerelease/build suffix; its byte spelling and suffix case are preserved.
   No leading `v`/`V`, token scan, first-token fallback, dependency-version
   filter, surrounding whitespace, or extra nonblank line is accepted. Both
   complete bounded streams are parsed independently before stream selection.
   Processing precedence is capture limit/read completion first, then signal
   or nonzero exit as the sole semantic failure for a completely captured
   process result. Only a zero exit proceeds to complete-stream UTF-8
   validation, blank classification, and whole-stream grammar. `ASCII blank`
   means the empty
   stream or bytes drawn only from HT `0x09`, LF `0x0a`, CR `0x0d`, and space
   `0x20`; VT, FF, NUL, nonbreaking space, and every other byte are nonblank.
   Implementations may not substitute `trim` or a library whitespace predicate.
   A selected product line still permits only optional final LF or CRLF, so a
   lone CR is legal only in the other blank stream. Exactly one may match while
   the other is ASCII blank. Two matching streams,
   even with the same version, are `ambiguous_version`; one match plus nonblank
   invalid output or any other nonblank mismatch is `unparseable_version`; two
   blank streams are `empty_output`. The exact bounded stdout and stderr byte
   digests and selected stream are retained only on success. No failure may
   yield `failures = []` or fabricate a normalized value.
10. **B-010:** Runtime environment evidence is derived from this exact closed
    v0.1 policy; callers provide values but never names, sensitivity, evidence
    inclusion, or probe exposure:

    | Runtime kind | Key | Evidence | Version-child exposure |
    | --- | --- | --- | --- |
    | `codex_exec`, `codex_jsonrpc` | `OPENAI_API_KEY` | `unset` or `redacted` | excluded |
    | `claude_code` | `ANTHROPIC_API_KEY` | `unset` or `redacted` | excluded |
    | `claude_code` | `CLAUDE_CONFIG_DIR` | `unset` or SHA-256 | excluded |
    | all three | `PATH` | domain-separated SHA-256 plus resolution outcome | exposed as the sanitized B-004 value |

    Present `PATH` and `CLAUDE_CONFIG_DIR` values use:
    `SHA-256(domain || platform_tag || unit_count_be || exact_units)`.
    Their domains are exactly
    `b"harness_runtime_environment_path_v0_1\0"` and
    `b"harness_runtime_environment_claude_config_dir_v0_1\0"`.
    `platform_tag`, count, and units use the exact B-004 encoding: `b"unix\0"`
    plus raw Unix bytes counted as bytes, or `b"windows\0"` plus original
    UTF-16 units counted by `u64` big-endian and serialized as little-endian
    `u16`. No normalization occurs. Unset is a distinct enum state and has no
    digest; present empty hashes the zero-unit encoding.

    Undeclared keys are ignored and can never enter evidence or the child.
    Setup-secret exclusion runs first and can remove even a listed policy key.
    Before duplicate, `PATH`, policy, or setup-secret matching, Unix compares
    keys exactly and case-sensitively. Windows v0.1 accepts only ASCII
    environment names, canonicalizes them to uppercase for comparison, rejects
    canonical collisions such as `Path` plus `PATH`, and serializes the policy
    spelling above; a non-ASCII Windows key fails typed rather than guessing OS
    comparison semantics. Raw values, raw paths, and directory contents never
    enter the envelope.

### Runtime Probe Failure Vocabulary

The table order below is normative. Phase ranks are `path_resolution = 0`,
`identity = 1`, `version_probe = 2`, and `lifecycle_cleanup = 3`; within each
phase, kind rank is the zero-based row order shown. Producers serialize failures
by `(phase_rank, kind_rank)` and parsers reject any other order. Thus concurrent
cleanup failures are always `termination_failed`, `reap_failed`, then
`output_drain_failed` when present. Observation-helper failures are typed
no-envelope producer errors defined in B-006 and never enter this list.

| Phase | Kind | Meaning |
| --- | --- | --- |
| `path_resolution` | `path_not_found` | No executable selected by the configured path/`PATH` launch contract. |
| `path_resolution` | `path_unusable` | The launch contract selected a path that cannot be represented or inspected safely. |
| `path_resolution` | `candidate_limit_exceeded` | Unix bare-name search reached candidate 65 before a terminal outcome; no further entry was observed. |
| `identity` | `open_failed` | A resolved candidate could not be opened as the retained inspection handle. |
| `identity` | `metadata_unavailable` | Required metadata or strong file identity could not be read from the opened handle. |
| `identity` | `not_regular_file` | The selected target is not a regular file. |
| `identity` | `not_executable` | Unix handle mode bits do not permit execution; Windows does not infer this fact from an extension or header. |
| `identity` | `executable_too_large` | The byte ceiling was exceeded before or during hashing. |
| `identity` | `read_failed` | The opened executable could not be read completely. |
| `identity` | `identity_changed` | Path strong identity, retained-handle size/content digest, or an already observed link count changed across checkpoints. |
| `version_probe` | `probe_not_authorized` | Configuration-source or resolved-target repository policy forbids executing this inspected target; the closed reason identifies which. Registered observation helpers may derive retained evidence, but no target child is created and no target, loader, or interpreter instruction runs. |
| `version_probe` | `target_authorization_unavailable` | The producer could not prove the opened target is outside every validated repository/worktree boundary or could not prove single-link ownership; the closed reason is `boundary_unprovable`, `link_count_unprovable`, `unlinked_target`, or `multiple_hard_links`. No target instruction ran. Initial/pre-spawn failure permits no target exec; retry failure requires exactly one prior retained-handle exec returning `ETXTBSY`, that helper reaped, and no second helper or exec. |
| `version_probe` | `interpreter_authorization_unavailable` | The retained executable was not a current-architecture static ELF without `PT_INTERP` or W+X `PT_LOAD` and with exactly one non-executable `PT_GNU_STACK`, or exact retained-handle exec-time `ENOENT`/`ENOTDIR` showed that a late interpreter contract could not be satisfied. No target, loader, or interpreter instruction ran; a late-race setup helper is reaped. |
| `version_probe` | `handle_execution_unavailable` | Mandatory ptrace containment passed, but the candidate's fully frozen fd-10 `execveat(AT_EMPTY_PATH)` returned exact `ENOSYS`, `EPERM`, or `EINVAL`; every created target helper was reaped and no target instruction ran. |
| `version_probe` | `supervision_setup_failed` | An initial/retry registered target failed working-directory entry or pre-exec ptrace stop/options setup; the closed stage is exactly `working_directory_enter` or `trace_setup`, the target was reaped, and handle exec was never attempted. |
| `version_probe` | `spawn_failed` | Direct exec of an inspected candidate failed terminally before start; no selected/executed claim is emitted. |
| `version_probe` | `transitive_execution_denied` | After the verified initial static image began under syscall-stop supervision, it attempted closed class `process_creation`, `image_execution`, `executable_mapping`, `executable_image_mutation`, `kernel_code_loading`, or `process_signalling`; the denied syscall never executed, stopped-target cleanup began, cleanup outcome is independent, and no version fact was emitted. |
| `version_probe` | `bare_eacces_exhausted` | Every inspected Unix bare-name exec attempt returned exact `EACCES`; no executable was selected or started. |
| `version_probe` | `timeout` | The probe deadline expired; cleanup outcome is represented independently. |
| `version_probe` | `output_limit_exceeded` | Combined stdout/stderr exceeded the inclusive hard byte limit; cleanup outcome is represented independently. |
| `version_probe` | `output_read_failed` | Either output pipe failed before a complete bounded result was obtained. |
| `version_probe` | `nonzero_exit` | The child exited with a nonzero code. |
| `version_probe` | `terminated_by_signal` | The child terminated without an exit code. |
| `version_probe` | `invalid_utf8` | Bounded output was not valid UTF-8. |
| `version_probe` | `empty_output` | Exit was successful but both streams were blank. |
| `version_probe` | `unparseable_version` | Nonblank output did not exactly match the selected runtime's whole-output grammar. |
| `version_probe` | `ambiguous_version` | Stdout and stderr each matched the selected grammar, so the selected stream was not unique. |
| `lifecycle_cleanup` | `termination_failed` | Signalling a registered target/helper pidfd failed before cleanup ownership was transferred. Observation-helper failures are producer errors and never enter an envelope. |
| `lifecycle_cleanup` | `reap_failed` | A registered target/helper reap failed or was not verified before cleanup ownership was transferred. Observation-helper failures are producer errors and never enter an envelope. |
| `lifecycle_cleanup` | `output_drain_failed` | Bounded drain did not complete before read handles were closed and ownership was transferred. |

### Unix Bare-Name Attempt Vocabulary

`RuntimeResolutionAttempt` exists for every Unix candidate. The payload's
closed `command_form` makes absolute and qualified commands distinguishable
from a single-entry bare search. Absolute and qualified commands have exactly
one entry; bare-name entries are ordered exactly like the first at most 64
sanitized `PATH` entries. Each attempt contains a candidate digest, one closed
exec sequence (`none`, `single`, or
`etxtbsy_then_checkpoint_after_150_ms`), one optional closed execution context,
and one closed outcome. The context is absent exactly when the sequence is
`none`; `single` and the retry sequence require exactly
`linux_fd_cloexec_execveat_empty_path_fd_10`, freezing child fd 10,
`FD_CLOEXEC`, empty path, `AT_EMPTY_PATH`, and resulting
`AT_EXECFN = "/dev/fd/10"`:

| Outcome | Meaning |
| --- | --- |
| `absent` | Preliminary observation or authoritative open returned exact `ENOENT`/`ENOTDIR`; a bare search continues, while the sole absolute/qualified attempt terminates as `path_not_found`. |
| `not_regular` | Opened candidate is not a regular file; a bare search skips without a global identity failure, while an absolute/qualified attempt is terminal and requires `identity/not_regular_file`. |
| `not_executable` | Opened candidate lacks required mode bits; a bare search skips without a global identity failure, while an absolute/qualified attempt is terminal and requires `identity/not_executable`. |
| `inspection_failed` | Open or later inspection failed; this is terminal and requires the matching B-008 identity failure. |
| `inspection_target` | Configuration-source or resolved-target repository policy retained this first authorized inspection identity and stopped without exec. |
| `authorization_unavailable` | Final-target repository/worktree classification or single-link ownership could not be proven; this is terminal and requires `target_authorization_unavailable` with exact reason `boundary_unprovable`, `link_count_unprovable`, `unlinked_target`, or `multiple_hard_links`. |
| `interpreter_authorization_unavailable` | Initial script/dynamic-ELF/unsupported-format detection uses `none`; exact exec-time `ENOENT`/`ENOTDIR` uses `single` or the one `ETXTBSY` retry sequence. It requires the matching version-probe failure, executes no target/loader/interpreter instruction, and is terminal. |
| `handle_execution_unavailable` | Mandatory ptrace containment passed but the fully frozen fd-10 call returned exact `ENOSYS`, `EPERM`, or `EINVAL`; this candidate-local terminal uses `single` for the first call or the one `ETXTBSY` retry sequence for the second, requires the fd-10 execution context and matching version-probe failure, and proves every created target helper was reaped before any target instruction ran. |
| `supervision_setup_failed` | An initial/retry registered target failed working-directory entry or ptrace setup, was reaped, and never attempted handle exec; this is terminal and requires the matching version-probe failure and exact `working_directory_enter` or `trace_setup` stage. |
| `retry_not_authorized` | After first exec returned `ETXTBSY`, the repeated checkpoint classified the target as repository-owned; requires `probe_not_authorized` with exact reason `resolved_target_repository` and forbids a second exec. |
| `retry_authorization_unavailable` | After first exec returned `ETXTBSY`, the repeated checkpoint could not prove target authorization; requires `target_authorization_unavailable` with the exact checkpoint reason `boundary_unprovable`, `link_count_unprovable`, `unlinked_target`, or `multiple_hard_links`, requires the first helper to be reaped, and forbids a second helper or exec. |
| `exec_verification_failed` | Native exec reached `PTRACE_EVENT_EXEC`, but stopped-image strong identity or retained-handle digest mismatched before the first instruction; requires `identity_changed`, uses `single` or the retry sequence, and proves the stopped child was killed/reaped without resume. |
| `exec_eacces` | Direct retained-handle exec returned exact `EACCES`; final identity is discarded and search continues. |
| `exec_failed` | Direct retained-handle exec returned another terminal error; requires `spawn_failed` and no selected/executed claim. |
| `exec_started` | Direct retained-handle exec reached a verified exec-stop and was resumed; this is the sole terminal selected executable. |

Parsers preserve list order and reject more than 64 attempts, an outcome after
a terminal outcome, any exec outcome after identity-only authorization,
multiple terminal outcomes, `inspection_target` without
`probe_not_authorized` and its closed source-or-target reason,
`authorization_unavailable` without `target_authorization_unavailable` and
exactly one of `boundary_unprovable`, `link_count_unprovable`,
`unlinked_target`, or `multiple_hard_links`,
`interpreter_authorization_unavailable` without its matching failure and legal
pre-exec or exec-time sequence,
`handle_execution_unavailable` without the matching
`version_probe/handle_execution_unavailable`,
`supervision_setup_failed` without the matching
`version_probe/supervision_setup_failed`,
`retry_not_authorized` without `probe_not_authorized` carrying exact reason
`resolved_target_repository`,
`retry_authorization_unavailable` without
`target_authorization_unavailable` and its exact checkpoint
`boundary_unprovable`, `link_count_unprovable`, `unlinked_target`, or
`multiple_hard_links` reason, one reaped first helper, and no second helper or
exec,
`exec_verification_failed` without `identity_changed` and verified no-resume
reap, `exec_failed` without `spawn_failed`, `exec_started` with a spawn failure, or
`bare_eacces_exhausted` unless the final non-skipped attempt is `exec_eacces`
and no final executable identity exists. `candidate_limit_exceeded` requires
`unix_bare` and exactly 64 nonterminal attempts. For `unix_bare`,
`path_not_found` permits only skipped `absent`, `not_regular`, and
`not_executable` outcomes and no identity failure. For `unix_absolute` or
`unix_qualified`, it permits exactly one `absent`; a sole `not_regular` or
`not_executable` is terminal and requires its matching identity failure.
The no-target-helper invariant of a `none` attempt is local to that attempt;
it does not erase a preceding reaped `exec_eacces` attempt. `none` is
required for outcomes that terminate before any direct exec;
an initial `inspection_failed` uses `none`.
An initial `supervision_setup_failed` or pre-observed shebang requires `none`;
`single` is required for an ordinary one-exec outcome, an exec-time interpreter
failure, initial-call `handle_execution_unavailable`, or
`exec_verification_failed`.
`etxtbsy_then_checkpoint_after_150_ms` requires an exact first `ETXTBSY` and a
repeated authorization/identity checkpoint. It permits
`retry_not_authorized`, `retry_authorization_unavailable`, or
`inspection_failed` without a second helper, and
`supervision_setup_failed` when the second registered target's retained
working-directory entry or trace setup fails before target handle exec;
exec-time `interpreter_authorization_unavailable`,
`handle_execution_unavailable`, `exec_verification_failed`, `exec_eacces` for
a bare name, `exec_failed`, or `exec_started` proves
the second target exec was attempted. It is rejected for every initial skip,
inspection-only, or authorization-unavailable outcome.
Absolute/qualified attempts reject `exec_eacces`, because their `EACCES` is
terminal `exec_failed`.
