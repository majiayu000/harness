# Product Spec

## Linked Issue

GH-1733

complexity: high

## User Problem

Harness can identify repository files and the prompt inputs it constructs, but
it cannot produce a bounded, reviewable fingerprint of the executable selected
for a configured agent runtime or of the tool contract advertised by an MCP
server. A file hash alone misses version output, launch-relevant environment
facts, MCP descriptions, and input schemas that can change agent behavior.

The fingerprint must remain honest about provenance. A runner performing an
observation does not become the owner of a repository-, user-, admin-, system-,
or runtime-owned component. Likewise, resolving or probing a configured
executable does not prove that an adapter later launched that same executable,
and exposing a producer API does not mean that Harness already collects these
facts in snapshots or through a CLI.

## Goals

- Produce versioned, deterministic ASC-001 evidence for configured local agent
  runtime executables and advertised MCP tool contracts.
- Resolve one configured executable with the adapter-compatible safe subset of
  its single-command and `PATH` semantics, with every fail-closed divergence
  documented and without enumerating or probing unrelated commands.
- Bound executable reads, child-process lifetime, and combined probe output,
  while recording every incomplete observation with a closed failure vocabulary.
- Preserve actual component ownership separately from runner observation and
  trust metadata.
- Canonicalize MCP schemas without erasing order that remains behavior-affecting
  inside annotations or ordered schema locations.
- Exclude setup-only secrets and persist only explicitly declared,
  behavior-affecting environment facts.

## Non-Goals

- Probing arbitrary `PATH` entries, shell expressions, package managers, MCP
  server processes, remote hosts, or executables not selected by configuration.
- Persisting credentials, setup-only secrets, raw environment values, a full
  process environment, or unbounded command output.
- Claiming that a local file hash and version probe are a cryptographic
  attestation or prove which bytes a later adapter invocation executed.
- Changing adapter launch arguments, sandboxing, capability enforcement,
  prompt construction, or current runtime selection.
- Automatically collecting fingerprints into an Agent Stack snapshot; ASC-005
  owns snapshot production and consumption.
- Adding a CLI, HTTP endpoint, persistence schema, or dashboard; ASC-026 owns
  native snapshot and diff commands.
- Fingerprinting Anthropic API or Remote Host as local executables, recording
  MCP calls, or inferring declared, granted, or observed capabilities.
- Accepting compatibility aliases, arbitrary string runtime kinds, or generic
  untyped fingerprint payloads.

## User-Visible Behavior

1. **B-001:** Every public fingerprint uses one strict outer envelope with
   schema version `agent-stack-fingerprint/v0.1`, a closed subject of
   `agent_runtime` or `mcp_tool`, one validated ASC-001 component, and the
   matching versioned typed payload. The runtime payload declares
   `runtime-executable-fingerprint/v0.1`; the MCP payload declares
   `mcp-tool-fingerprint/v0.1`. Public serialization and parsing consume this
   envelope, reject missing or unknown versions, subjects, and fields, and
   cannot pair a subject with the other payload type. Each envelope also has a
   required `fingerprint_digest` over its versioned subject and canonical
   payload. This digest is separate from ASC-001 `component.integrity`, whose
   meaning remains SHA-256 of exact source bytes or absence. The v0.1 component
   capability list is always empty because this evidence does not infer
   capabilities; constructors emit an empty list and parsers reject a nonempty
   one. No exported schema constant exists without a corresponding wire field
   and parser check.
2. **B-002:** A runtime fingerprint accepts a closed typed local-executable
   kind with exactly `codex_exec`, `codex_jsonrpc`, and `claude_code`. These
   values correspond one-to-one to the locally launched workflow
   `RuntimeKind` variants without adding a dependency from `harness-agents` to
   `harness-workflow`; any later consumer performs an exhaustive typed mapping
   at its boundary. `anthropic_api`, `remote_host`, unknown strings, aliases,
   case variants, UUIDs, display labels, and caller-supplied hex encodings
   cannot be converted into a local runtime identity. Version arguments come
   from the closed mapping for the selected kind and are not an arbitrary
   public command vector. The same mapping owns a whole-output version grammar:
   both Codex kinds accept only `codex-cli <VERSION>`, and Claude Code accepts
   only `<VERSION> (Claude Code)`, apart from one optional final line ending.
   Runtime producer input also carries the existing closed execution isolation
   value and the adapter's effective `SandboxSpec`. v0.1 accepts only `host`
   plus the exact passthrough sandbox state
   `DangerFullAccess` with `allowed_write_paths = None`; its payload records the
   closed fact `sandbox_policy = danger_full_access_unrestricted`.
   `container` and `microvm` retain their typed unsupported-isolation errors.
   `ReadOnly`, `ReadOnlyWithNetwork`, `WorkspaceWrite`, or any narrowed
   allowed-write-path state fails separately with
   `sandbox_parity_unavailable`. Both gates precede PATH resolution,
   working-directory or executable access, and process creation. v0.1 never
   runs an unrestricted version child for an adapter launch that would be
   restricted.
3. **B-003:** Every producer accepts or derives a validated ASC-001 source from
   persisted ownership information. Repository-, user-global-, admin-, system-,
   and runtime-owned runtime or MCP components retain their source scope and
   persisted ownership identity when observed by a runner; the closed runtime
   and MCP bindings below may derive a typed locator from that base identity.
   Only a genuinely runner-owned executable or tool definition uses `runner`
   source scope. The emitted observation class, trust, selection state, and
   freshness are `runner_observed`,
   `runner_observed`, `observed`, and `fresh`; stronger observation never
   rewrites the component ID. Invalid, ambiguous, or untyped ownership fails
   before a fingerprint is emitted. This bounded producer accepts a validated
   ASC-001 base source locator of at most 4,096 UTF-8 bytes and a complete
   derived runtime/server/tool locator of at most 8,259 UTF-8 bytes. These are
   fingerprint-local limits, not changes to global ASC-001 validity. All
   derivation uses checked length arithmetic and validates before suffix
   allocation or copying. `from_exact_source_bytes` accepts at most 2,097,152
   source bytes and checks byte 2,097,153 before copying or SHA-256 work; this
   fingerprint-local constructor limit does not change global ASC-001 validity.
   Strict envelope input is at most 2,097,152 raw bytes and is rejected before
   JSON allocation when larger. Exact limits are accepted; limit-plus-one fails
   with the matching closed limit kind. The closed
   `RuntimeFingerprintLimitKind` values are `exact_source_bytes`,
   `base_source_locator_bytes`, `derived_source_locator_bytes`, and
   `envelope_bytes`. Raw-envelope size has first precedence before JSON
   allocation; strict parsing then checks complete derived-locator size before
   suffix decoding and recovered-base size. Exact-source construction checks
   source byte length before base size, checked suffix expansion, complete
   derived size, copying, or hashing; construction without source bytes begins
   at the base-size check.
   Runtime component identity is a typed role
   binding derived internally from the base source and exact B-002 runtime-kind
   spelling:
   `<base_locator>/harness_agent_runtime_role_v0_1/u<byte_length>_<lowercase_utf8_hex>`.
   This injective mapping preserves scope and gives `codex_exec`,
   `codex_jsonrpc`, and `claude_code` distinct component IDs even when two
   roles share one persisted base binding. Callers cannot supply or pre-encode
   the role locator. Envelope parsing strips the final two locator segments,
   validates the remaining base source under the same scope, decodes the exact
   length-prefixed suffix, requires one canonical runtime-kind spelling equal
   to the payload kind, re-derives the locator, and rejects missing, malformed,
   noncanonical, or wrong-role suffixes. If the base binding has exact
   canonical source bytes, each derived role retains their identical ASC-001
   integrity; otherwise integrity is absent. Role text and fingerprint payload
   never enter component integrity.
4. **B-004:** Runtime resolution treats the configured executable as exactly
   one command name or path and never invokes a shell or parses embedded
   arguments, quoting, pipes, substitutions, or redirections. Windows mirrors
   the frozen Windows v0.1 search behavior below, independent of the compiler
   used to build Harness; Unix uses an explicit safe subset that preserves
   `EACCES` fallback but deliberately rejects `ENOEXEC` shell fallback. On
   Unix, a qualified relative path and
   relative or empty `PATH` entries are resolved from the declared child
   working directory. Every payload records one closed `command_form`:
   `unix_bare`, `unix_absolute`, `unix_qualified`, `windows_bare`,
   `windows_absolute`, or `windows_qualified`; strict parsing uses it to
   distinguish search skips from failures of a configured path.

   Every payload includes
   `configured_command_digest = SHA-256("harness_runtime_configured_command_v0_1\0" || platform_tag || unit_count_be || exact_units)`.
   `platform_tag` is exactly `b"unix\0"` or `b"windows\0"`;
   `unit_count_be` is an unsigned fixed-width `u64` in big-endian order.
   Unix exact units are the raw `OsStr` bytes; Windows exact units are the
   original UTF-16 code units serialized little-endian, and the count is UTF-16
   units rather than bytes. There is no UTF-8,
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

   On Windows, a bare name follows the frozen v0.1 search
   order and `.exe` completion rules using explicit launch-context inputs; it
   does not use `PATHEXT`, and `.bat`/`.cmd` are `path_unusable` because the
   adapter's batch handling would invoke a command interpreter contrary to the
   no-shell boundary. Every
   explicitly named non-`.exe` extension is `path_unusable` in v0.1. A
   qualified relative path or relative/empty search input whose base cannot be
   proven is also `path_unusable`.

   The payload carries exactly four optional Windows resolution-context fields:
   `current_executable_dir_digest`, `system_dir_digest`,
   `windows_dir_digest`, and `parent_path_digest`. Each present field is
   `SHA-256(domain || b"windows\0" || u64be(utf16_unit_count) ||
   utf16le_units)` under its field-specific
   `harness_runtime_windows_search_<field>_v0_1\0` domain frozen in the tech
   spec. Absent is distinct from present empty. These fields enter the
   fingerprint payload; raw directories and parent PATH never do.

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
   `interpreter_authorization_unavailable` before anchor creation so neither an
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
   pre-anchor result on a later candidate preserves every earlier ordered
   attempt, including a fully reaped `exec_eacces` helper, while proving that
   the terminal candidate itself created no anchor or target.
   The anchor is probe-local rather than attempt-local: once the first direct
   exec attempt creates it, it remains the live group leader across bare-name
   fallback. Every later terminal outcome, including a pre-anchor classifier
   rejection or candidate-limit failure, must prove the group contains only
   that anchor, request anchor exit, and reap it under the active/finalization
   deadline. Failure appends the canonical lifecycle-cleanup evidence and
   retains exact ownership; it never rewrites the terminal attempt.

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
   `TOKEN` or `SECRET`; no setup-only or undeclared value can reach
   `codex --version` or an equivalent child. The producer counts and bounds
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
   mutation syscall is stopped before kernel execution and fails closed. Thus
   an allowed static target cannot use PATH, cwd, `dlopen`, `/proc/self/mem`,
   asynchronous I/O submission, or a child process to execute
   repository/worktree code.
7. **B-007:** A version probe has one lifecycle covering authorization, spawn,
   concurrent stdout/stderr reads, exit, timeout, cleanup, and root reap.
   Authorization is the conjunction of configuration-source policy and opened
   target policy. Repository-owned configuration and any opened executable
   proven inside a validated repository/worktree boundary are identity-only:
   `probe_not_authorized` carries the closed reason
   `configuration_source_repository` or `resolved_target_repository`, and no
   child is spawned. User-global, admin, system, runtime, and genuine runner
   sources are eligible only when the opened target is proven outside every
   validated boundary. Missing or ambiguous final-target/boundary evidence is
   `target_authorization_unavailable` and prevents spawn. Callers cannot
   override either decision with a boolean, path label, or trust string. Stdout
   and stderr are read incrementally under one
   inclusive hard combined byte limit. The validated public value is in
   `1..=65_536`; values outside that range fail typed before allocation or
   process creation. Exactly `max_output_bytes` is allowed,
   while observing byte `max_output_bytes + 1` triggers
   `output_limit_exceeded`.

   On Linux after isolation/sandbox and the static platform gate, before cwd
   or executable observation, any supervision helper, or
   any target child, v0.1 first atomically `try_acquire`s one of exactly eight
   process-global owner permits. Capacity exhaustion is typed no-envelope
   `containment_unavailable/owner_capacity_exhausted` before a thread, fd, cwd
   open, helper, or child and never waits or falls back. An admitted request
   starts a fingerprint-specific runtime-independent cleanup owner and receives
   a readiness handshake under the separate fixed
   `RUNTIME_FINGERPRINT_OWNER_READY_DEADLINE = 1_000 ms`, measured monotonically
   from immediately before thread creation. The owner bootstrap performs no
   blocking work before sending readiness and observing its cancellation-aware
   control channel. Thread creation, closed handshake, or deadline expiry closes
   that channel and starts a separate fixed
   `RUNTIME_FINGERPRINT_OWNER_STOP_JOIN_DEADLINE = 1_000 ms` from the stop
   request. The bootstrap owner is joined within that second bound. Failure
   returns typed producer error `containment_unavailable` with no envelope and
   one closed reason
   (`owner_start_failed`, `owner_ready_timeout`, or
   `owner_stop_join_timeout`) without creating a child. Caller cancellation
   during reservation follows the same bounded stop/join path and emits no
   envelope. The permit moves to a created owner and is released only after its
   thread actually exits and every helper/child obligation is reaped; API
   return, cancellation, deadline expiry, cleanup-incomplete, or stop/join
   timeout cannot release it early. A thread that never started returns the
   caller-held permit. After descriptor isolation reports
   `descriptors_ready`, each owner retains at most 67 Harness-owned pidfds
   (anchor, root, one current helper, and one 64-member batch); a membership
   helper may retain the 64 transferred member references, so one descriptor-
   isolated fingerprint is bounded at 131 retained pidfd references after READY
   and eight are bounded at 1,048 after READY. Each owner has an exact 32-slot
   non-pidfd ledger covering control,
   gate, descriptor-ready/status, protocol, anchor, cwd/executable,
   stdin/stdout/stderr, pre-exec, observation, membership-transfer, and
   relocation/rollback descriptors. Each role's child allowlist is at most 12
   non-pidfd descriptors, giving post-READY retained ceilings of 44 per
   fingerprint and 352 across eight. Pre-READY fork inheritance is bounded only
   in time by the registration/cleanup deadline and exact positive-PID rollback;
   none of the numeric retained-reference ceilings claims that transient
   interval. A batch is disposed before another is accepted.
   Capacity violation is typed no-envelope
   `owner_resource_capacity_exceeded` with closed reason `pidfds` or
   `non_pidfd_fds` and retains the permit with all existing obligations. Every
   logical slot is reserved before fd creation, fork, `pidfd_open`, or
   `SCM_RIGHTS`; logical exhaustion wins over an injected OS resource failure,
   while a post-reservation `EMFILE` is the applicable child-registration
   stage. After readiness and
   before the first cwd open, the producer starts
   the fixed nonzero
   `RUNTIME_FINGERPRINT_PROBE_DEADLINE = 5_000 ms`. That one monotonic budget
   covers every observation subprocess, cwd and executable observation,
   authorization, static-ELF classification, traced exec-stop setup/
   verification, anchor and target
   setup, every target-child attempt, the optional 150 ms `ETXTBSY` delay/retry,
   output collection, root exit, and the post-reap identity checkpoint.
   Expiry during an observation subprocess returns the typed producer error
   above and no envelope, so required payload facts and attempt states are
   never fabricated. Any timeout, missing/surplus exec event, or trace
   verification failure before a verified `exec_started` resume returns typed
   producer error `execution_verification_unavailable` or the applicable
   observation error with no envelope; the stopped child remains owner-managed
   and is killed/reaped without resume. Only expiry after verified resume
   records `version_probe/timeout`. Cleanup receives its own separate
   five-second deadline.

   Linux v0.1 requires descriptor-table isolation, a successful owner-side
   `pidfd_open(getpid(), 0)` plus `pidfd_send_signal(..., 0)` preflight,
   `pidfd_send_signal`, parent-child ptrace with `PTRACE_O_TRACEEXEC` and
   `PTRACE_O_TRACESYSGOOD`, exact `PTRACE_GET_SYSCALL_INFO` inspection at
   syscall-entry stops, and strong `/proc`
   process/image identity enumeration before host filesystem observation;
   other Unix platforms return that typed no-envelope error without opening the
   cwd or executable. Missing tagged syscall-stop/syscall-info capability is
   no-envelope
   `containment_unavailable/post_exec_guard_unavailable` before cwd observation.
   The ready owner thread is the sole target/anchor fork,
   parent-side ptrace-control, wait/reap, and observation-helper-spawn owner;
   the target's audited pre-exec `PTRACE_TRACEME` call is the sole exception.
   Before creating that capability child, the owner performs the self-pidfd
   preflight without touching cwd or another filesystem path and closes the
   test pidfd. Any syscall failure is the typed no-envelope
   `containment_unavailable/pidfd_unavailable`; it can never become a
   capability-child `child_registration_unavailable`. After a successful
   preflight, an individual child's later `pidfd_open` resource or registration
   failure remains `child_registration_unavailable/pidfd_open`.
   The pre-observation capability child is instead traced only by owner-side
   `PTRACE_SEIZE/PTRACE_INTERRUPT`; it never calls `PTRACE_TRACEME`.
   Before every capability/observation/membership helper, anchor, initial
   target, or retry target fork, the owner reserves all logical slots, then
   creates a close-on-exec start gate, descriptor-ready/status channel, and
   role-specific fixed descriptor allowlist. Before gate wait, the child uses
   only raw allocation-free syscalls to map stdio, close every non-allowlisted
   inherited fd with segmented `close_range`, and emit one closed bootstrap
   status: `descriptors_ready`, `descriptor_isolation_unavailable`, or
   `descriptor_isolation_failed`. Until the parent receives READY, opens and
   commits the pidfd/reap
   obligation, and sends one-byte `GO`, the child cannot touch
   cwd/filesystem/proc, join a group, ptrace, inspect, or exec. Registration
   failure or cancellation closes the gate, and the exact still-unreaped
   direct-child positive PID is used only for bounded rollback before reap; no
   negative PGID or post-reap PID action is allowed. Completed rollback returns
   typed no-envelope `child_registration_unavailable` with closed child role
   and gate/fork/descriptor-isolation/pidfd-open/registry/gate-release stage;
   incomplete rollback retains the exact
   obligation and owner permit under
   `child_registration_cleanup_incomplete` with closed gate-close/termination/
   reap operation. For the initial capability child only, exact
   `ENOSYS`/seccomp `EPERM` or `EACCES`/unsupported-flag `EINVAL` maps after reap
   only to `containment_unavailable/descriptor_isolation_unavailable`; another
   initial isolation error and every post-capability isolation failure map only
   to `child_registration_unavailable/descriptor_isolation`. A concrete status
   precedes deadline; no status before deadline uses the deadline error, and
   cancellation emits no result while rollback continues. No lease or
   descriptor is exposed.
   After the verified initial `PTRACE_EVENT_EXEC`, the owner resumes only with
   `PTRACE_SYSCALL` and classifies each entry stop before the syscall executes.
   The closed denied classes are `process_creation` (`fork`, `vfork`, `clone`,
   `clone3`), `image_execution` (`execve`, `execveat`),
   `executable_mapping` (`mmap`/`mmap2`/`mprotect`/`pkey_mprotect` requesting
   `PROT_EXEC`, or `shmat` requesting `SHM_EXEC`), and
   `executable_image_mutation`. The last class covers `ptrace`,
   `process_vm_writev`, `userfaultfd`, `io_uring_setup`, `pidfd_getfd`,
   `recvmsg`, `recvmmsg`, `prctl`, every non-query `personality`, `creat`, and
   `open`/`openat`/`open_by_handle_at`/`openat2` whose decoded flags request
   `O_WRONLY`, `O_RDWR`, or `O_TRUNC`; an unreadable or noncanonical
   `openat2` `open_how` is execution-verification unavailable rather than
   allowed. Because the frozen target descriptor table contains no pre-opened
   writable memory fd or Unix socket, this blocks `/proc/self/mem` and alias
   write-open paths, received `SCM_RIGHTS`, remote-fd duplication,
   external-ptrace authorization, and `READ_IMPLIES_EXEC` changes without
   trusting pathname inspection. The sole allowed `personality` form is the
   side-effect-free exact `0xffff_ffff` query. A denied entry is never
   executed: the owner records `version_probe/transitive_execution_denied` with
   only that closed class and starts exact stopped-target cleanup.
   Termination/reap failure is represented independently by the normative
   lifecycle-cleanup kinds while the owner retains the stopped obligation.
   Missing, surplus, untagged, or unreadable syscall stops return no-envelope
   `execution_verification_unavailable`; no version or transitive-containment
   claim is emitted. The initial static image needs no post-exec executable
   mapping; a runtime that requires threads, a helper process, dynamic loading,
   or JIT execution is deliberately unrepresentable in v0.1.
   The owner creates and retains a dedicated process-group anchor, creates every
   target through this gate, records target pidfd/reap ownership, and
   establishes the ptrace relationship before exposing a cancellable client
   lease. Target
   children join that anchored
   group before exec. The owner holds pidfds for the anchor, root, current
   helper, and only the current bounded non-anchor member batch. Every `/proc` group enumeration and
   revalidation runs in an owner-created, atomically pidfd-registered
   observation helper under the active or cleanup deadline; no such filesystem
   work runs on the owner or async runtime. Each bounded pass transfers at most
   64 exact revalidated non-anchor pidfds plus a `more` bit. The owner signals
   and disposes the transferred batch before accepting another, then rescans
   `/proc` from the beginning until a full
   pass reports only the anchor; it never uses a reusable PID cursor or drops
   members beyond a full batch. Continuous churn reaches the applicable
   deadline rather than claiming empty. Timeout or incomplete helper cleanup
   returns its closed observation error; malformed protocol/helper exit returns
   a distinct closed protocol-invalid error; cancellation uses owner cleanup.
   All return no envelope, including when probe cleanup had already begun. The
   owner signals those pidfds individually. It never sends a signal
   to a negative PGID, so group-wide
   termination cannot kill the anchor. Success requires proving that no
   non-anchor member remains, then requesting anchor exit over its private
   control channel and reaping it. Only after the group is proven empty may a
   failed anchor control/reap path signal the anchor through its own pidfd.
   After anchor reap, code performs no lookup, signal, or ownership decision
   using the released numeric PGID. A platform unable to provide that
   identity-safe membership proof returns the typed no-envelope error before
   cwd observation or target exec. v0.1 claims only
   `process_group_supervision`: root reap and visibility of members that remain
   in the anchored original group. A child can call `setsid` or change groups,
   so v0.1 does not claim non-escapable descendant containment or whole-process-
   tree emptiness. On Linux platforms that pass the mandatory ptrace guard,
   exact `ENOSYS`, `EPERM`, or `EINVAL` from the reached eligible candidate's
   fully frozen fd-10 `execveat(AT_EMPTY_PATH)` call records
   `handle_execution_unavailable` after every created target helper is reaped.
   Failure to establish the anchor group or failure of either
   the initial or post-`ETXTBSY` target helper's pre-exec group join or
   retained-working-directory `fchdir` is `supervision_setup_failed` with the
   closed `setup_stage` `group_join`, `working_directory_enter`, or
   `trace_setup`; anchor
   failure uses `anchor_setup`. Every created helper is reaped, the affected
   target handle exec is not attempted, and its errno is never treated as a
   target-exec error or PATH fallback signal. On Windows, where the existing launcher cannot atomically
   assign a Job Object before execution, the typed no-envelope
   `containment_unavailable` producer error is returned without spawning.

   Every terminal path, including a zero exit with apparently valid output,
   reaps the root and verifies that only the live anchor remains before success
   is possible, then exits/reaps the anchor without another group-membership
   operation. A
   non-anchor member still present after root exit records
   `lingering_process_group`, suppresses version success, and starts group
   termination. An observation-helper timeout runs bounded exact-helper pidfd
   termination/reap and returns the closed no-envelope producer error defined
   in B-006. Separately, probe timeout, overflow, read failure, and any
   lingering non-anchor member run bounded member pidfd signalling, pipe drain,
   root reap where still needed, and anchored membership verification under one
   fixed five-second monotonic cleanup deadline. If all
   complete, only the triggering probe failure is recorded. If signalling,
   drain, or reap fails or the deadline expires during one of those operations,
   the applicable closed `lifecycle_cleanup` failure is also recorded, read
   handles are closed, and the already-reserved runtime-independent cleanup
   owner retains sole ownership before the API returns. Membership-verification
   timeout, cleanup-incomplete, malformed response, or unexpected helper exit
   instead returns its closed no-envelope observation error with the same
   retained ownership. That owner emits an `error`, retains
   ownership, and continues exact-pidfd signalling, reaping, and original-group
   verification; it never silently abandons a child. The separate producer
   error path gives the same ownership guarantee for observation helpers. A
   process stuck in kernel uninterruptible I/O may outlive both deadlines, so
   no result fabricates a termination guarantee. There is no on-demand
   cleanup-thread creation or synchronous unbounded drop fallback. Caller
   cancellation activates the same pre-existing owner but emits no fingerprint.
   Root-only
   `kill_on_drop` and the existing detached `ManagedChild` reaper are not
   completion evidence. Output is never fully buffered and then truncated, and
   no lifecycle or cleanup failure contains a version fact.
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
   Processing order is capture limit, complete-stream UTF-8 validation, blank
   classification, then whole-stream grammar. `ASCII blank` means the empty
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
11. **B-011:** An MCP tool producer requires a typed configured-server binding
    made from a validated ownership source and the exact stable key of a
    persisted MCP configuration entry; it does not accept an arbitrary
    prebuilt `mcp_server` component, display label, UUID, or session identity.
    The stable key is at most 1,024 UTF-8 bytes; the exact limit is accepted
    and limit-plus-one fails typed before hexadecimal expansion or locator
    allocation. After UTF-8 validation it is rejected as blank exactly when it
    is empty or every byte is HT (`0x09`), LF (`0x0a`), CR (`0x0d`), or SP
    (`0x20`). There is no trimming or Unicode whitespace predicate; VT, FF,
    NBSP, and every other valid UTF-8 scalar are nonblank and remain exact
    identity bytes. The advertised tool-name nonblank check uses the same
    predicate.
    The binding derives the server locator as
    `<base_locator>/harness_mcp_server_config_v0_1/u<byte_length>_<lowercase_utf8_hex>`
    from that exact nonblank UTF-8 key. The tool producer also requires the
    exact advertised tool name; callers cannot supply a separate or pre-encoded
    server or tool source. A typed v0.1 mapping preserves the server source
    scope and derives the tool locator as
    `<server_locator>/harness_mcp_tool_v0_1/u<byte_length>_<lowercase_utf8_hex>`.
    The length-prefixed exact UTF-8 encoding is reversible, injective, and
    case-sensitive, so multiple tools from one server cannot collide. Runner
    observation describes who obtained the contract and never changes ownership
    scope. The derived structured tool binding has no canonical raw source
    bytes, so ASC-001 component integrity is absent. Strict parsing validates
    both derived suffixes and requires the payload server ID and tool name to
    match them. Blank stable keys or names, wrong server kind, malformed or
    noncanonical derived locators, payload mismatches, and caller-supplied
    encodings fail closed. This contract binds identity to the exact stable
    configuration key; it does not claim historical proof that a caller kept
    the same key across observations.
12. **B-012:** MCP tool name, optional description, optional `annotations`,
    required `inputSchema`, and optional `outputSchema` are fingerprinted as
    advertised. Absence of `outputSchema` remains distinct from any present
    schema. Both schemas use
    the same duplicate-aware, object-root, bounded canonicalization contract in
    B-013. `annotations` accepts only absent or a raw JSON object through the
    same duplicate-aware parser; absent and `{}` remain distinct. Its object
    keys canonicalize, while all values use `InstanceData`, so exact booleans
    such as `readOnlyHint`, `destructiveHint`, `idempotentHint`, and
    `openWorldHint`, exact title/vendor values, and array order affect the
    digest without populating the empty ASC capability list. Tool text is
    retained in its exact UTF-8 string value; the producer
    does not trim,
    collapse, case-fold, Unicode-normalize, or otherwise rewrite whitespace or
    punctuation. `None`, an empty description, a single space, repeated spaces,
    tabs, and newlines remain distinct payload facts and produce distinct
    digests when their exact advertised values differ. v0.1 permits a nonblank
    tool name of at most 1,024 UTF-8 bytes and a description of at most 65,536
    UTF-8 bytes. Annotation limits are fixed independently at 65,536 raw bytes,
    49,152 canonical bytes, depth 32, 4,096 JSON nodes, 32,768 cumulative
    decoded-string UTF-8 bytes, and 1,024 entries in one object or array. Exact
    limits are allowed and limit-plus-one fails typed before fingerprint
    construction.
13. **B-013:** Every present MCP input or output schema must have a JSON object
    at its root; a root boolean, array, string, number, or null fails typed
    before canonicalization or digest construction. Boolean schemas remain
    legal only at schema-valued child positions. The root dialect is frozen
    before keyword traversal: absent `$schema` means Draft 2020-12; the only
    accepted explicit values are exactly
    `https://json-schema.org/draft/2020-12/schema` and
    `http://json-schema.org/draft-07/schema#`. A non-string, unknown value, or
    `$schema` at any nested schema position fails typed
    `unsupported_schema_dialect`; nested dialect changes are outside v0.1.
    The exact `$schema` member remains canonical payload data.

    JSON object member order is canonicalized lexicographically. In both
    dialects, arrays at `required`, `type`, `enum`, `allOf`, `anyOf`, and
    `oneOf` are order-insensitive under their closed contexts. Draft 2020-12
    additionally treats `$defs`, `dependentSchemas`, `dependentRequired`,
    `prefixItems`, `contentSchema`, and `unevaluatedItems` /
    `unevaluatedProperties` according to that dialect; `dependentRequired`
    property arrays are string sets, `prefixItems` remains an ordered schema
    array, and `items` accepts only an object or boolean schema. In both
    dialects, `not`, `if`, `then`, `else`, `contains`, `propertyNames`, and
    `additionalProperties` are single object-or-boolean schema positions even
    when a neighboring activating keyword is absent. Other value shapes fail
    typed `malformed_single_schema_keyword` with a closed dialect/keyword
    detail, and nested `$schema` at any of these positions is rejected. In
    Draft-07, `definitions`, legacy `dependencies`, and array-form `items`
    receive their legacy schema or string-set semantics; `additionalItems`
    always remains a schema-valued keyword, even without array-form `items`.
    Under Draft-07 `contentSchema`, `dependentRequired`, `dependentSchemas`,
    `prefixItems`, and other later-draft keywords are extension instance data;
    under Draft 2020-12 `dependencies`, `definitions`, `additionalItems`, and
    array-form `items` are extension instance data or a typed malformed
    standard keyword as applicable. Thus Draft-07 `contentSchema` arrays retain
    order and cannot be collapsed by Draft 2020-12 keyword rules.

    Unknown/vendor-extension values and instance-valued annotations including
    `default`, `const`, `examples`, and `example` preserve array order at every
    depth. A key named `enum`, `required`, or `oneOf` inside instance data is
    ordinary data and does not activate schema sorting. Public evidence
    construction accepts only raw JSON text or bytes through a duplicate-aware
    parser. It exposes no `serde_json::Value`, generic serializable, or
    typed-map constructor that could claim duplicate-free original input after
    an ordinary decoder overwrote a key. `harness-core` explicitly enables the
    existing `serde_json/raw_value` feature. The private visitor borrows each
    member/element as `RawValue`, recursively visits its validated source slice,
    and retains number-leaf `RawValue::get()` text so `1`, `1.0`, `1e0`, and
    long valid tokens remain distinct. No handwritten lexer, new package, or
    lockfile change is permitted.

    Resource limits are fixed by v0.1 and are not caller-adjustable. For each
    present schema: raw bytes are at most 1,048,576; canonical bytes at most
    786,432; nesting depth at most 64; total JSON nodes at most 65,536;
    cumulative decoded-string UTF-8 bytes at most 524,288; and any single
    object or array at most 4,096 entries. Exact limits are valid.

    Depth is the number of JSON value nodes on a root-to-value path, with the
    root at depth 1. Every JSON value, including the root, object member value,
    and array element, counts as one node; object keys are not nodes. Decoded
    string bytes count each object key and string value after JSON unescaping,
    once per occurrence. Container entries count direct object members or array
    elements. On entering a value the visitor checks depth, then increments and
    checks nodes; before accepting a member/element it increments and checks
    that container's entries; after decoding a key or string value it adds and
    checks its UTF-8 bytes. Raw size is checked first; canonical-byte budget is
    checked only after duplicate-aware parse and structural budgets succeed.
    A limit-plus-one condition returns its exact closed typed limit error,
    emits no digest or envelope, and cannot panic or overflow the stack.
14. **B-014:** The envelope `fingerprint_digest` covers the exact subject,
    payload schema version, and every behavior-affecting typed payload fact.
    Hash input is frozen as
    `domain || u64be(subject_len) || subject_utf8 ||
    u64be(inner_version_len) || inner_version_utf8 ||
    u64be(payload_len) || canonical_payload_utf8`, where `domain` is exactly
    `b"harness_agent_stack_fingerprint_digest_v0_1\0"` and every length counts
    bytes. Canonical payload JSON has no insignificant whitespace; object keys
    sort by decoded UTF-8 bytes; arrays retain typed order unless B-013 declares
    the location a set; `null`, booleans, and typed integers use
    lowercase/minimal JSON spelling; strings escape only quote, backslash, and
    U+0000..U+001F, using `\b`, `\t`, `\n`, `\f`, `\r` where applicable and
    lowercase `\u00xx` otherwise, while other Unicode scalars are emitted as
    UTF-8 and slash is never escaped. Raw MCP JSON number tokens are validated
    and preserved byte-for-byte, so `1`, `1.0`, and `1e0` remain distinct
    contract facts rather than passing through a floating-point formatter.
    The normative payload bytes `{"a":1,"z":"\n"}` (hex
    `7b2261223a312c227a223a225c6e227d`) produce digest
    `3f45cc1b14c0099eaf056f9475aa210b4f84d45b2a4940ecff35079b3b1611fe`
    for subject/version `agent_runtime` /
    `runtime-executable-fingerprint/v0.1`, and
    `e00eca6b5f5a3fe3494cf590e68ec59f70e40ee54b7f7f42e48756d296fa85d9`
    for `mcp_tool` / `mcp-tool-fingerprint/v0.1`. These framing vectors are
    independent of production serialization; tests also pin one complete valid
    payload vector for each subject. The digest excludes timestamps, run IDs,
    localized diagnostics, raw secret values, and the outer ASC-001 component
    to avoid self-reference. ASC-001 component integrity
    independently preserves exact-source-byte evidence when one exists and is
    absent otherwise; payload or failure changes never fabricate or overwrite
    it. Reordering JSON object members or a B-013 order-insensitive set leaves
    the fingerprint digest unchanged; changing runtime facts, exact MCP
    description text, an ordered annotation, or a failure kind changes it.
    Aggregate ordering and stack IDs remain ASC-005 responsibilities.
15. **B-015:** Invalid producer inputs, invalid source evidence, unsupported
    runtime kinds, non-host isolation, malformed or over-limit schema JSON, and
    impossible envelope combinations return typed errors and emit no
    fingerprint. Expected observation failures such as a missing executable,
    unauthorized repository probe, unavailable version, or incomplete cleanup
    are successful evidence records only when they contain the applicable
    B-008 failure and omit every unsupported fact. Missing data remains absent;
    no empty digest, sentinel path, placeholder version, runner-owned alias,
    fingerprint-as-integrity substitution, or warning-only fallback is used.
16. **B-016:** This issue exposes deterministic producer APIs in
    `harness-core` and `harness-agents` plus contract tests. It does not invoke
    them from `CodeAgent`, `AgentAdapter`, the workflow runtime, task runner,
    server startup, snapshot assembly, or a command. Existing execution and
    public wire behavior remain unchanged until ASC-005 or ASC-026 adds an
    explicit consumer. Documentation and tests say “can produce” rather than
    claiming that Harness already collected or used a fingerprint.

### Runtime Probe Failure Vocabulary

The table order below is normative. Phase ranks are `path_resolution = 0`,
`identity = 1`, `version_probe = 2`, and `lifecycle_cleanup = 3`; within each
phase, kind rank is the zero-based row order shown. Producers serialize failures
by `(phase_rank, kind_rank)` and parsers reject any other order. Thus concurrent
cleanup failures are always `termination_failed`, `reap_failed`, then
`output_drain_failed` when present. Membership-verification failures are the
typed no-envelope observation errors defined in B-006 and never enter this
list.

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
| `version_probe` | `probe_not_authorized` | Configuration-source or resolved-target repository policy forbids executing this inspected target; the closed reason identifies which and no child was started. |
| `version_probe` | `target_authorization_unavailable` | The producer could not prove the opened target is outside every validated repository/worktree boundary or could not prove single-link ownership; the closed reason is `boundary_unprovable`, `link_count_unprovable`, `unlinked_target`, or `multiple_hard_links`. No target instruction ran. Initial/pre-spawn failure permits no target exec; retry failure requires exactly one prior retained-handle exec returning `ETXTBSY`, that helper reaped, and no second helper or exec. |
| `version_probe` | `interpreter_authorization_unavailable` | The retained executable was not a current-architecture static ELF without `PT_INTERP` or W+X `PT_LOAD` and with exactly one non-executable `PT_GNU_STACK`, or exact retained-handle exec-time `ENOENT`/`ENOTDIR` showed that a late interpreter contract could not be satisfied. No target, loader, or interpreter instruction ran; a late-race setup helper is reaped. |
| `version_probe` | `handle_execution_unavailable` | Mandatory ptrace containment passed, but the candidate's fully frozen fd-10 `execveat(AT_EMPTY_PATH)` returned exact `ENOSYS`, `EPERM`, or `EINVAL`; every created target helper was reaped and no target instruction ran. |
| `version_probe` | `supervision_setup_failed` | Anchor setup or an initial/retry target helper group join, working-directory entry, or pre-exec ptrace stop/options setup failed; the closed setup stage identifies which, every created helper was reaped, and target handle exec was never attempted. |
| `version_probe` | `spawn_failed` | Direct exec of an inspected candidate failed terminally before start; no selected/executed claim is emitted. |
| `version_probe` | `transitive_execution_denied` | After the verified initial static image began under syscall-stop supervision, it attempted closed class `process_creation`, `image_execution`, `executable_mapping`, or `executable_image_mutation`; the denied syscall never executed, stopped-target cleanup began, cleanup outcome is independent, and no version fact was emitted. |
| `version_probe` | `bare_eacces_exhausted` | Every inspected Unix bare-name exec attempt returned exact `EACCES`; no executable was selected or started. |
| `version_probe` | `lingering_process_group` | The root exited but a non-anchor member remained in the anchored Unix process group; version success was suppressed and cleanup started. |
| `version_probe` | `timeout` | The probe deadline expired; cleanup outcome is represented independently. |
| `version_probe` | `output_limit_exceeded` | Combined stdout/stderr exceeded the inclusive hard byte limit; cleanup outcome is represented independently. |
| `version_probe` | `output_read_failed` | Either output pipe failed before a complete bounded result was obtained. |
| `version_probe` | `nonzero_exit` | The child exited with a nonzero code. |
| `version_probe` | `terminated_by_signal` | The child terminated without an exit code. |
| `version_probe` | `invalid_utf8` | Bounded output was not valid UTF-8. |
| `version_probe` | `empty_output` | Exit was successful but both streams were blank. |
| `version_probe` | `unparseable_version` | Nonblank output did not exactly match the selected runtime's whole-output grammar. |
| `version_probe` | `ambiguous_version` | Stdout and stderr each matched the selected grammar, so the selected stream was not unique. |
| `lifecycle_cleanup` | `termination_failed` | Signalling an exact root, non-anchor member, or post-empty-group anchor pidfd failed before cleanup ownership was transferred. Observation-helper failures are producer errors and never enter an envelope. |
| `lifecycle_cleanup` | `reap_failed` | Root or anchor reap failed or was not verified before cleanup ownership was transferred. Observation-helper failures are producer errors and never enter an envelope. |
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
| `supervision_setup_failed` | Anchor setup or an initial/retry target helper group join, working-directory entry, or ptrace setup failed, every created helper was reaped, and the affected target never attempted handle exec; this is terminal and requires the matching version-probe failure and exact setup stage. |
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
it does not erase a preceding reaped `exec_eacces` attempt or its probe-level
anchor. `none` is required for outcomes that terminate before any direct exec;
an initial `inspection_failed` uses `none`.
An initial `supervision_setup_failed` or pre-observed shebang requires `none`;
`single` is required for an ordinary one-exec outcome, an exec-time interpreter
failure, initial-call `handle_execution_unavailable`, or
`exec_verification_failed`.
`etxtbsy_then_checkpoint_after_150_ms` requires an exact first `ETXTBSY` and a
repeated authorization/identity checkpoint. It permits
`retry_not_authorized`, `retry_authorization_unavailable`, or
`inspection_failed` without a second helper, and
`supervision_setup_failed` when the second helper's group join, retained
working-directory entry, or trace setup fails before target handle exec;
exec-time `interpreter_authorization_unavailable`,
`handle_execution_unavailable`, `exec_verification_failed`, `exec_eacces` for
a bare name, `exec_failed`, or `exec_started` proves
the second target exec was attempted. It is rejected for every initial skip,
inspection-only, or authorization-unavailable outcome.
Absolute/qualified attempts reject `exec_eacces`, because their `EACCES` is
terminal `exec_failed`.

## Acceptance Criteria

- [ ] Public positive and negative fixtures round-trip both strict B-001
      envelope subjects and reject version, subject, payload, and unknown-field
      mismatches, including a component with any nonempty capability list;
      fixtures prove fingerprint digest is separate from ASC-001 integrity and
      failure changes never fabricate or overwrite exact-source-byte integrity.
- [ ] Runtime APIs accept only the closed typed local-executable kind, use fixed
      version arguments, and cannot launder arbitrary strings, UUIDs, display
      aliases, or pre-encoded hex into component identity; an exhaustive
      mapping test covers every workflow `RuntimeKind` without reversing the
      crate dependency direction. Container and microVM inputs fail before any
      host resolution, file access, or process creation. Restricted sandbox
      modes and any allowed-write-path narrowing fail
      `sandbox_parity_unavailable` at the same boundary; only the exact
      passthrough sandbox state is serialized.
- [ ] Source fixtures prove repository-, user-, admin-, system-, runtime-, and
      genuinely runner-owned runtime/MCP components keep identical component
      IDs when observation strengthens to `runner_observed`; repository-owned
      runtime fixtures retain identity/hash evidence but emit
      `probe_not_authorized` without executing a marker program, and callers
      cannot promote that source to an executable trust class. Non-repository
      source fixtures resolving inside a repository/worktree boundary do the
      same, while missing or ambiguous target-boundary evidence emits
      `target_authorization_unavailable`; neither case starts a child or falls
      through to another PATH candidate. Runtime-role
      fixtures prove the three derived IDs are pairwise distinct for one base
      source, preserve scope and identical exact-source integrity or absence,
      and cannot be caller pre-encoded. Strict parser fixtures reject missing,
      malformed, noncanonical, and payload-wrong role suffixes. All
      runtime/server/tool bindings accept 4,096-byte base locators and reject
      4,097 before copying, use checked derivation, and reject complete
      locators above 8,259 bytes. A maximum base plus maximum stable key and
      tool name reaches exactly 8,259 bytes. Strict envelope parsing accepts
      2,097,152 raw bytes and rejects byte 2,097,153 before JSON allocation;
      exact source construction accepts 2,097,152 bytes and rejects byte
      2,097,153 before copy or SHA-256;
      fixtures pin the four closed limit reasons and their precedence.
- [ ] PATH fixtures prove Unix child-working-directory semantics and Windows
      frozen search order/`.exe` behavior, including Windows refusal to use
      `PATHEXT`, accept any explicitly named non-`.exe` extension, execute
      `.bat`/`.cmd` through a shell, or guess a qualified-relative base; both
      platforms cover an absent command, duplicate basenames, spaces, literal
      shell metacharacters, one absolute selected probe path, and no unrelated
      executable. Unix bare-name fixtures prove exact `EACCES` fallback to a
      later same-basename candidate, one exact 150-millisecond `ETXTBSY` retry
      with a fresh authorization/identity checkpoint, terminal second
      `ETXTBSY`, terminal `ENOEXEC` without `/bin/sh`, terminal other errors,
      and no search fallback for absolute or qualified commands. Injected open
      denial is `open_failed`, mutually exclusive with handle-based failures,
      and never leaks OS diagnostics. Exact `ENOENT`/`ENOTDIR` during open
      remains `absent`; bare search continues, while an absolute/qualified sole
      candidate becomes `path_not_found`. Existing non-regular or
      mode-ineligible absolute/qualified paths retain their matching identity
      failure instead of masquerading as not found. Separate late-race
      retained-handle `ENOENT` and `ENOTDIR`
      fixtures prove the fingerprint stops with
      `interpreter_authorization_unavailable`, does not execute a later PATH
      candidate, and records the documented security divergence even when the
      adapter would continue searching.
- [ ] Unix bare-name PATH-unset fixtures return `path_unusable` without
      observing a default search path, while absolute and qualified commands
      remain representable; Windows conformance compares the frozen resolver
      against the adapter's current `Command` behavior and fails on drift
      instead of adopting it.
- [ ] Launch-input fixtures accept 65,536 and reject 65,537 exact OS units for
      every closed value field before hashing/splitting/owner admission.
      Checked joins accept the mathematically maximal 196,610-unit Unix lexical
      candidate; no public input can reach 196,611 after those field gates.
      Environment keys/setup-secret names and their collection
      counts accept 1,024 and reject 1,025 before canonicalization or value
      access. They cover Unix non-UTF8 bytes, Windows surrogate-pair UTF-16
      counting, one huge PATH entry, whole PATH, and every closed limit reason.
      A 65-entry PATH selecting or terminally rejecting an earlier entry never
      reaches the candidate ceiling, while 64 nonterminal attempts followed by
      entry 65 yields `candidate_limit_exceeded`. Excluded over-limit
      PATH/Claude values and
      undeclared over-limit values are never read, hashed, or reported as value
      limits; selected counterparts fail closed. Limit failure produces no
      digest, owner, fd, cwd observation, helper, child, truncation, or fallback.
- [ ] Unix attempt fixtures round-trip every closed outcome, reject illegal
      source/outcome/failure/terminal combinations, preserve duplicate PATH
      entries and order, cover exactly 64 candidates and candidate 65, and
      prove `bare_eacces_exhausted` retains no final executable identity.
- [ ] Repository bare-name fixtures with an injected first-candidate `EACCES`
      condition and a second marker prove only the first static inspection
      target is hashed, no exec attempt or fallback occurs, and no
      selected/executed claim is emitted.
- [ ] Configured-command digest fixtures freeze Unix raw-byte and Windows
      UTF-16LE vectors, distinguish absent commands and spelling/path variants
      under otherwise identical search facts, and prove raw command text is
      absent from serialized evidence. Independent hard-coded vectors also
      freeze the exact platform tags, `u64` big-endian counts, candidate digest
      domain, and Windows little-endian `u16` units.
- [ ] Windows resolution-context fixtures freeze the four exact fields, domains,
      absent/present states, UTF-16LE framing, and independent `C:\X` vectors
      for current-executable directory, system directory, Windows directory,
      and parent PATH.
- [ ] Linux launch fixtures prove bare, qualified, and absolute probes use
      `FD_CLOEXEC` retained-handle `execveat(AT_EMPTY_PATH)`, stop at
      `PTRACE_EVENT_EXEC` before the first target instruction, and resume only
      after stopped-image identity plus retained-handle hash match while
      preserving exact configured command bytes as `argv[0]`. A call-site
      audit proves the owner is the sole target/anchor fork, parent-side ptrace
      controller, wait/reap, and observation-helper-spawn authority; the
      target pre-exec closure's audited `PTRACE_TRACEME` is the sole exception.
      After verified initial exec, syscall-entry fixtures prove `fork`, `vfork`,
      `clone`, `clone3`, `execve`, `execveat`, executable `mmap`/`mprotect`,
      executable `shmat`, `ptrace`, `process_vm_writev`, `userfaultfd`,
      `io_uring_setup`, `pidfd_getfd`, `recvmsg`/`recvmmsg`, `prctl`,
      non-query `personality`, and write-capable/truncating open-family requests are
      stopped before kernel execution, yield the exact closed
      transitive-denial class, execute no second-image/child/mapping/mutation
      marker, and suppress version. Exact `/proc/self/mem`, thread-self,
      numeric-pid, and mount-alias write opens are denied without pathname
      parsing; read-only opens remain legal. Received `SCM_RIGHTS`, remote-fd
      duplication, external-ptrace authorization, and both mmap and mprotect
      `READ_IMPLIES_EXEC` attempts are denied. The exact
      `personality(0xffff_ffff)` query is resumed, returns through the ordinary
      entry/exit trace transition, and does not produce denial evidence; every
      other argument is denied. Exact and extended `openat2`
      payloads are decoded, while short, unreadable, unknown-tail, and
      unknown/conflicting-flag payloads return no-envelope execution
      verification failure. Missing/untagged/unreadable syscall stops return no
      envelope and cleanup the target.
      A static aux-vector fixture proves each direct-exec attempt records
      `linux_fd_cloexec_execveat_empty_path_fd_10`, observes exact
      `AT_EXECFN = "/dev/fd/10"`, cannot reopen fd 10 after exec, and does not
      misstate pathname-launch parity. A preceding reaped first-candidate
      `EACCES` followed by a second-candidate pre-anchor classifier rejection
      preserves both attempt records and applies the no-child invariant only
      to the second attempt. It also proves the pre-existing probe anchor exits
      and is reaped; injected termination and reap failures append independent
      lifecycle-cleanup evidence and retain ownership, while membership-helper
      timeout/protocol failure returns its typed no-envelope observation error.
      The owner opens the
      declared child directory once, fixed working-directory spelling/identity
      digests enter the payload, both initial and retry helpers `fchdir` that
      retained handle, and injected `fchdir` failure is stage-tagged, reaped,
      and cannot exec or PATH-fallback. Replacing the configured cwd pathname
      after that open proves qualified and relative-PATH observation, initial
      open, both checkpoints, and initial/retry helpers all remain anchored to
      the same retained directory identity.
- [ ] Linux executable-format fixtures prove static `ET_EXEC` and static-PIE
      `ET_DYN` success for the exact native machine tuple. Direct and
      `#!/usr/bin/env` shebangs, `PT_INTERP`, same-class/endianness
      wrong-machine ELF, wrong header version/size, W+X `PT_LOAD`, missing,
      duplicate, or executable `PT_GNU_STACK`, extended or out-of-bounds program headers, and
      non-ELF/binfmt inputs yield
      `interpreter_authorization_unavailable` before anchor, target, loader, or
      interpreter creation. Neither sanitized `PATH` nor a setup-only secret
      named `NPM_ACCESS` can select or reach an interpreter. A supported static
      native-binary fixture separately proves sanitized `PATH` is the only
      child environment key and that any later PATH/cwd helper launch or
      executable mapping is denied before execution.
- [ ] Identity fixtures prove handle-based metadata/hash consistency, before-
      and during-read fixed 67,108,864-byte size limit plus limit-plus-one,
      kill-isolated observation subprocess execution, Unix
      nonblocking-open rejection of FIFOs and other special files, Unix
      mode-bit checks, Windows strong file-ID checks without a fabricated
      executable claim, pre-spawn/post-reap retained-handle rehashing, and
      explicit `identity_changed` evidence for path replacement or in-place
      rewrite. Initial in-repository/outside hard-link aliases fail
      `multiple_hard_links` before spawn; an unlinked retained target is the
      distinct `unlinked_target` reason, and unavailable link metadata is the
      distinct `link_count_unprovable` reason. New or removed links at
      pre-spawn, retry, exec-stop, and post-reap checkpoints produce the
      stage-appropriate authorization or identity failure. Retry fixtures
      require one reaped `ETXTBSY` helper and prove no second helper or exec,
      while a stable single-link target remains eligible. Exact 0/1/2 counts
      and unavailable exec-stop/post-reap observations exercise the closed
      outcomes above. These tests do not claim bind-mount alias exclusion.
      Delayed cwd open, candidate open, boundary lookup, read/hash, and
      both later checkpoint fixtures prove the active deadline starts before
      observation, every observation timeout returns a typed producer error
      with no envelope, the producer future returns boundedly, no
      `spawn_blocking` job survives inside the process, and an unreaped helper
      stays owned with explicit incomplete cleanup rather than a termination
      claim. Stalled `/proc` group enumeration under both active and cleanup
      deadlines follows the same no-envelope rule. Membership fixtures accept
      exactly 64 transferred pidfds, drain 65 and larger groups through
      from-the-beginning rescans, reject descriptor-count and `more` protocol
      mismatches distinctly from timeouts, and fail closed under continuous
      churn. Cancellation at every create/register boundary, including both
      membership stages, proves the owner has the pidfd before any lease is
      exposed.
- [ ] Authorization-race fixtures replace and rewrite the source after the
      final checkpoint and prove the exec-stop hash/identity gate kills a
      changed native image before its first instruction, while an introduced
      shebang fails before interpreter execution. After mandatory ptrace
      containment passes, exact `ENOSYS`, `EPERM`, and fixed-call `EINVAL` from
      `execveat(10, "", ..., AT_EMPTY_PATH)` each emit an inspected
      `handle_execution_unavailable` envelope, reap every created helper, and
      never fall back to a pathname. Fixtures cover each errno on the initial
      call and after exact first-call `ETXTBSY`, requiring `single` and the
      retry sequence respectively; exact `EACCES` remains the separate
      bare-name fallback case. Missing ptrace exec-stop or tagged syscall guarding instead
      returns no-envelope
      `containment_unavailable/post_exec_guard_unavailable` before cwd
      observation. Other
      platforms return typed no-envelope `containment_unavailable` first under
      the frozen matrix. Separate missing-event, surplus-event, abnormal-trace,
      and pre-resume-deadline fixtures return
      `execution_verification_unavailable` with no envelope and prove the owner
      kills/reaps without ever resuming the stopped target.
- [ ] Lifecycle fixtures use hanging and unbounded dual-stream children to
      reject output caps 0 and 65,537 before allocation or helper creation and
      prove 1, 65,536, exact combined limit, and limit-plus-one behavior; owner
      readiness succeeds within its fixed one-second deadline or stops/joins
      within a separate one-second deadline, including cancellation,
      delayed-handshake, and typed stop/join-timeout fixtures; after readiness
      the active five-second deadline starts before cwd observation and covers
      all observation helpers, traced exec-stop verification, anchor creation,
      root exit, and post-reap
      checkpoint, while the separate cleanup deadline starts at failure;
      ordinary supported-Linux cleanup finishes helper/root/anchored-group
      handling within five seconds; injected
      initial and post-`ETXTBSY` group-join, termination, reap, and drain
      failures, including post-empty-group anchor termination/reap failure,
      produce canonical `lifecycle_cleanup` evidence and transfer ownership
      without a version; membership verification timeout/protocol failure
      instead returns the closed no-envelope observation error; an escaped
      `setsid` pipe holder cannot yield
      whole-tree-empty evidence or block the API past the cleanup deadline;
      a zero-exit child that leaves a same-group marker process records
      `lingering_process_group`, suppresses version success, and runs the same
      cleanup; cancellation transfers ownership to a runtime-independent reaper
      that survives immediate Tokio shutdown and emits no evidence. Linux
      fixtures prove exact pidfds are revalidated and only non-anchor members
      are signalled, no negative-PGID signal exists, the anchor exits only after
      the group is empty, and no released PGID is used. macOS, other Unix, and
      Windows prove typed no-envelope `containment_unavailable` is returned
      before cwd observation or child creation.
- [ ] Owner-capacity fixtures retain exactly eight permanently stalled owners
      and prove the ninth fails before thread/fd/cwd/child creation; API return,
      cancellation, cleanup-incomplete, and stop/join timeout do not release a
      permit, while actual owner exit does. Concurrent admission never exceeds
      eight. Each owner stays within 67 pidfds and 32 other fds, each membership
      helper stays within 64 pidfds, and after `DESCRIPTORS_READY` retained
      transfer stays within 131 per fingerprint and 1,048 globally; the
      pre-READY transient is asserted only to meet its deadline/rollback bound.
      One 64-member batch is disposed
      before another. Two-owner and eight-owner interleavings inspect
      `/proc/<pid>/fd` at `DESCRIPTORS_READY` and find exactly the role allowlist;
      foreign-fd markers prove a stalled helper retains no other owner's gate,
      output, or control fd and cannot delay its EOF or bounded rollback.
      Owner-side self-pidfd fixtures cover successful preflight and each
      `pidfd_open`/signal-zero failure, prove the test descriptor closes, and
      require `pidfd_unavailable` before the capability-child fork or cwd
      observation. Start-gate fault injection covers every observation stage, anchor, initial
      target, and retry target: bootstrap unavailable, post-capability isolation
      failure, missing READY until deadline, cancellation, and forced
      `pidfd_open`/registry/gate-release failure. None runs a child marker or
      filesystem/group/ptrace/exec work before `GO`; each then either
      reaps the exact gated direct child or retains its closed cleanup obligation
      and owner permit. Logical slot exhaustion plus injected `EMFILE` returns
      resource capacity; reserved slots plus `EMFILE` return the exact
      registration stage.
- [ ] Failure fixtures cover every B-008 phase/kind, deterministic ordering,
      sanitized bounded facts, rejection of unknown values, and a fixed vector
      that orders simultaneous cleanup kinds as termination, reap, then
      output-drain regardless of observation order.
- [ ] Version fixtures cover exact current Codex and Claude product lines,
      stdout/stderr selection, prerelease/build case preservation, CRLF/LF,
      rejected `v`/`V`, leading/trailing text, extra dependency versions,
      nonzero and signalled exit, invalid UTF-8, successful blank output,
      one valid stream plus nonblank invalid output, same/different versions on
      both matching streams, unparseable output, and conflicting matching
      streams. Blank fixtures cover empty and every mixture of HT/LF/CR/SP as
      allowed, VT/FF/NUL/NBSP as nonblank, lone CR as legal only in the
      unselected blank stream, and invalid UTF-8 precedence; implementation
      audits forbid generic whitespace predicates.
- [ ] Environment fixtures prove the runtime-kind policy table's
      set/unset/digest/redacted behavior, arbitrary and cross-runtime keys
      cannot be declared or exposed, raw-value and raw-PATH absence, setup
      secrets override the closed policy, Unix comparison remains
      case-sensitive, and Windows canonical comparison rejects `Path`/`PATH`
      collisions and non-ASCII keys. Independent hard-coded PATH and
      `CLAUDE_CONFIG_DIR` vectors freeze both exact domains, platform tags,
      `u64` counts, Unix raw bytes, Windows UTF-16LE units, absent versus empty,
      and non-UTF-8 Unix input.
- [ ] MCP fixtures prove exact stable configuration-key binding, distinct
      server IDs for distinct keys, injective typed source derivation for
      multiple exact UTF-8 tool names on one server, rejection of arbitrary
      prebuilt server components and caller-supplied encoded sources, strict
      server/tool suffix parsing, absent tool component integrity, exact
      tool/description sensitivity, and distinct absence, empty, spaces, tabs,
      and newlines; exact 1,024-byte and rejected 1,025-byte stable keys are
      checked before locator expansion. Stable keys and tool names reject empty
      or all-HT/LF/CR/SP bytes, preserve mixed nonblank input exactly, and treat
      VT, FF, and NBSP as nonblank without trimming. Absent, empty, standard-hint, title,
      vendor-value, and ordered-array annotation fixtures remain distinct under
      the bounded raw-object contract without inferring ASC capabilities.
- [ ] Schema fixtures cover required `inputSchema` and absent, present,
      malformed, non-object, exact-limit, and limit-plus-one `outputSchema`;
      reject every non-object root before canonicalization and
      prove object-key and approved set reordering stability. Absent and exact
      Draft 2020-12 roots agree; exact Draft-07 roots select legacy semantics;
      unknown/non-string root or any nested `$schema` fails typed. In Draft
      2020-12, ordered `prefixItems`, `contentSchema` nested schemas,
      `dependentRequired`, and modern schema maps are context-aware while
      legacy keys remain extension data. In Draft-07, array `items`,
      `additionalItems`, `definitions`, and `dependencies` use legacy
      semantics while a `contentSchema` value with ordered arrays remains
      instance data; `additionalItems` traverses schema context even without
      tuple `items`. In both dialects every `not`, `if`, `then`, `else`,
      `contains`, `propertyNames`, and `additionalProperties` object/boolean
      child traverses schema context, rejects another value shape with its
      closed keyword detail, rejects nested `$schema`, and canonicalizes nested
      sets. The same key names under vendor extensions, `default`, `const`,
      `examples`, and `example` remain ordered instance data and digest-
      sensitive in both dialects.
- [ ] Duplicate JSON keys and malformed raw schemas fail typed before digesting,
      and public API/call-site audit proves no generic serializable or
      `serde_json::Value` evidence constructor exists. The core manifest
      explicitly enables `serde_json/raw_value`; borrowed recursive
      `RawValue::get()` tests distinguish `1`, `1.0`, `1e0`, long valid tokens,
      malformed number syntax, and the canonical long-number boundary without
      a handwritten lexer or lockfile change.
- [ ] Tool-name, description, annotation, raw-schema, depth, node,
      decoded-string, per-container, and canonical-byte limit fixtures cover
      exact limits and limit-plus-one. Independent fixtures pin root depth one,
      value-only node counts, decoded object-key plus string-value bytes, and
      direct container entries; deep/wide hostile input fails typed without a
      digest, envelope, panic, unbounded allocation, or stack overflow.
- [ ] Independent digest fixtures freeze the exact B-014 domain, all three
      `u64` frames, canonical string escaping, raw JSON number-token
      preservation, both normative framing vectors, and one complete valid
      payload vector per subject.
- [ ] Focused and package suites pass with
      `cargo check -p harness-agents -p harness-core --all-targets`,
      `cargo test -p harness-core fingerprint`, and
      `cargo test -p harness-agents runtime_fingerprint`.
- [ ] A changed-file and call-site audit proves there is no snapshot, CLI,
      server, workflow-runtime, `CodeAgent`, or `AgentAdapter` consumer and no
      persistence or existing launch-behavior change. The only manifest change
      is a direct `harness-agents` dependency on the existing workspace `libc`;
      no new package, version, or lockfile change is allowed.

## Boundary Checklist

| Boundary | Verdict |
| --- | --- |
| Empty / missing input | Covered by B-002, B-009, B-011, B-012, and B-015; blank identities and output are explicit errors or failure evidence. |
| Error and failure paths | Covered by B-006 through B-009 and B-015; every incomplete observation uses the closed vocabulary. |
| Authorization / permission | Covered by B-004, B-005, B-007, B-010, and B-016; repository-owned configuration and resolved repository/worktree targets are never executed, ambiguous target ownership fails closed, and the producer gains no shell, caller policy, snapshot, or runtime authority. |
| Concurrency / race / ordering | Covered by B-006, B-007, B-008, B-013, and B-014; handle identity, child lifecycle, and canonical ordering are explicit. |
| Retry / repetition / idempotency | Covered by B-008 and B-014; unchanged bounded facts produce identical records and digests. |
| Illegal state transitions | N/A. Producers are stateless; B-001 and B-015 reject impossible evidence combinations. |
| Compatibility / migration | Covered by B-001, B-002, and B-016; this is additive producer-only code with no persisted or existing public wire migration. |
| Degradation / fallback | Covered by B-008, B-009, and B-015; unavailable data becomes typed failure evidence, never a placeholder success. |
| Evidence and audit integrity | Covered by B-003 and B-006 through B-015; ownership, observer, bytes, version, schema, failures, and redaction remain distinguishable. |
| Cancellation / interruption / partial completion | Covered by B-006, B-007, and B-015; cancellation or deadline expiry signals exact helper/root/non-anchor pidfds, transfers cleanup to a runtime-independent owner, and cannot publish partial success evidence. |

## Edge Cases

- The configured command is a bare name and two `PATH` directories contain it.
- A qualified relative executable contains spaces or shell metacharacters.
- On Unix, an npm-style launcher uses `#!/usr/bin/env node`; v0.1 detects the
  shebang and fails before searching sanitized child `PATH` or starting an
  interpreter.
- A setup-only secret has a harmless-looking name such as `NPM_ACCESS`.
- Windows observation input contains `Path` and `PATH`, a case-variant setup
  secret, or a non-ASCII environment key.
- A repository-owned marker executable would write or access the network if
  invoked; fingerprinting must stop after identity/hash evidence.
- A user-global binding resolves to a repository executable, or final-target
  containment cannot be proven; neither case may start a child.
- The executable is a symlink, is replaced, is overwritten in place, or is
  changed and restored between observation checkpoints.
- A candidate path names a FIFO, socket, directory, or device and must return
  without a blocking read open.
- Cwd or executable observation stalls in kernel I/O; the producer deadline
  returns a typed error and no envelope while the independent owner retains the
  exact helper pidfd and no in-process blocking worker survives.
- A regular file grows past the byte limit after its first metadata read.
- A probe hangs, forks, closes only one stream, floods both streams, exits by
  signal, succeeds with blank output, or emits conflicting versions.
- A zero-exit probe leaves a child in the original process group after closing
  both output streams.
- A probe forks a `setsid` descendant that retains an output pipe beyond the
  cleanup deadline; evidence must not claim the whole descendant tree is empty.
- Version output contains invalid UTF-8 or valid Unicode surrounding an
  otherwise valid product line; both are explicit failures.
- An MCP description differs only by tabs, repeated spaces, or line breaks.
- A schema annotation contains `{ "enum": [1, 2] }` as ordinary default data.
- `prefixItems` or a vendor extension differs only by array order.
- Draft 2020-12 and Draft-07 schemas use the same extension key with different
  keyword semantics; nested `$schema` is rejected rather than switching
  dialect mid-document.
- Tool text or schema input is exactly at, then one unit beyond, every fixed
  v0.1 resource limit.
- An MCP tool is advertised by a runner but owned by repository configuration.
- One MCP server advertises two exact tool names that differ only by case,
  Unicode bytes, slash placement, or a prefix of the other name.
- A caller attempts to use a UUID, display label, or arbitrary string as a
  runtime kind or source locator.

## Rollout Notes

The producer APIs are additive and initially have no automatic consumer, so
they require no migration or feature flag and cannot alter current execution.
ASC-005 may later place these envelopes into canonical snapshots, and ASC-026
may expose snapshot collection through native CLI commands. Those consumers
must preserve the B-003 ownership/observation split and must not reinterpret a
probe record as proof that an adapter executed the inspected bytes.
