# Runtime Supervision Design

## Linked Spec Packet

This document defines GH-1733 runtime probe ownership, exact-pidfd lifecycle,
post-exec guarding, output handling, and version parsing. Read it with
`product.md`, `runtime-product.md`, `runtime-observation.md`, `tech.md`, and
`tasks.md`.

## Supervised Version Probe

Before owner admission, the producer validates `max_output_bytes` in
`1..=65_536`. Invalid values fail before allocation, cwd observation, or
process creation. Supported Linux then atomically `try_acquire`s one of eight
global owner permits; it never waits or degrades. The runtime-independent owner
performs a closed readiness handshake within
`RUNTIME_FINGERPRINT_OWNER_READY_DEADLINE = 1_000 ms`. A stop request uses a
separate `RUNTIME_FINGERPRINT_OWNER_STOP_JOIN_DEADLINE = 1_000 ms`. The closed
start, ready-timeout, and stop/join reasons return no envelope. A created owner
retains the permit until its thread exits and its exact pidfd registry is empty.

The owner reserves exactly two pidfd slots: one current target and one current
capability or observation helper. Eight owners therefore retain at most 16
pidfds. Its exact non-pidfd ledger contains 28 slots. Before
`DescriptorsReady`, at most one newly forked bootstrap child per owner may
transiently retain the process-wide inherited descriptor table in addition to
an already admitted target role. That transient has no numeric ceiling derived
from the owner ledger and performs no workload. The active deadline bounds the
wait for `DescriptorsReady`; expiry starts exact direct-child rollback under
the cleanup deadline. No descriptor-reference release is claimed until reap,
and cleanup-incomplete retains the obligation and permit. After
`DescriptorsReady`, foreign
references are absent and simultaneous allowlisted child references are frozen
by phase:

| Phase | Live child references |
| --- | --- |
| ready bootstrap or ordinary observation | one gated/helper child, at most 12 |
| target after verified exec, without observer | exactly three stdio references |
| exec-stop observation | target's three stdio plus at most five observer references, total at most eight |

No other phase permits two live child roles. The post-ready child-reference
maximum is therefore 12. The
post-`DescriptorsReady` retained ceilings are 28 + 12 = 40 per fingerprint
and 320 globally; they do not bound the transient inherited table. There is no
process-group anchor, PGID slot,
membership helper, membership-transfer channel, or member batch. Capacity is
reserved before an fd, fork, `pidfd_open`, or `SCM_RIGHTS` operation.
Logical-capacity failure wins over an injected OS allocation failure.

Immediately after readiness and before cwd access, the owner spends one
reserved slot on `pidfd_open(getpid(), 0)` and
`pidfd_send_signal(self_pidfd, 0, NULL, 0)`, then closes it. Failure is
`ContainmentUnavailable(PidfdUnavailable)`. It next runs one bounded
capability helper behind the standard start gate. The helper proves descriptor
isolation, owner-side `PTRACE_SEIZE/PTRACE_INTERRUPT`,
`PTRACE_O_TRACEEXEC | PTRACE_O_TRACESYSGOOD | PTRACE_O_EXITKILL`, tagged
syscall stops, `PTRACE_GET_SYSCALL_INFO`, and safe architecture-specific
syscall suppression. It then exercises an actual fixed trusted native
retained-handle exec, exact `PTRACE_EVENT_EXEC`, and strong stopped-image
identity plus offset-zero hash observation through the production observation
protocol. It also
attempts a write-capable open of `/proc/self/mem`; the owner must classify and
deny that entry before execution. The helper never calls `PTRACE_TRACEME`.
Containment is not available until this successful
`Observation(CapabilityCheck)` child exits and the owner validates its
terminal event with `waitid(P_PIDFD, ..., WEXITED | WNOWAIT)`, then consumes
and validates the same event with `waitid(P_PIDFD, ..., WEXITED)`. Both
results must name the registered child identity with `CLD_EXITED` and status
zero.

Every capability helper, observation helper, initial target, and retry target
uses the same creation transaction:

1. reserve the role's exact fd slots;
2. create a close-on-exec start gate and fixed status channel;
3. fork one direct child;
4. have the child close non-allowlisted descriptors and report its closed
   descriptor-isolation status without touching cwd, `/proc`, or the target;
5. open the direct child's pidfd and atomically register the pidfd plus reap
   obligation;
6. send `GO` only after registration succeeds.

Before step 5 completes, registration failure closes the gate and may signal
only the exact still-unreaped positive PID of that direct child for bounded
rollback. Before rollback, the owner registry records a pre-registration PID
obligation. A missed termination/reap deadline retains that obligation and the
permit until exact direct-child reap; registry emptiness includes it. After
registration, every signal decision is tied to the registered
pidfd identity. The initial capability child additionally proves pidfd wait and
reap before that mechanism is trusted. A `WNOWAIT` error/mismatch, or a
consuming-call error while the child remains unreaped, permits the sole
fail-closed bootstrap reap by exact positive PID while its pidfd remains held.
A successful consuming wait has reaped the child, so a malformed result then
permits no PID operation. Completed rollback returns `PidfdUnavailable`;
fallback failure returns the existing cleanup-incomplete error and retains the
obligation and owner permit. Success enables exact-pidfd-only wait/reap for
every later registered child. No operation uses a negative
PID, PGID, a PID after reap, or `/proc` membership. Cancellation cannot
observe a lease before the registry commit.

One monotonic
`RUNTIME_FINGERPRINT_PROBE_DEADLINE = Duration::from_millis(5_000)` begins
before cwd observation. It covers every helper, retained cwd and executable
operation, authorization checkpoint, ELF classification, trace setup,
`PTRACE_EVENT_EXEC` verification, initial or retry target, the 150 ms
`ETXTBSY` delay, bounded output reads, exact target exit/reap, and the
post-reap identity checkpoint. Observation-helper timeout, malformed protocol,
unexpected exit, or incomplete cleanup returns the corresponding typed
no-envelope producer error. Expiry before verified resume kills/reaps the
registered stopped target without resume and returns
`ExecutionVerificationUnavailable`; expiry after resume records
`version_probe/timeout`. Once envelope-capable post-resume semantic cleanup has
reaped the target, the mandatory post-reap identity helper uses a fresh cleanup
deadline so the failure envelope can retain only freshly verified executed
identity.

The target's allocation-free pre-exec closure is the sole production
`PTRACE_TRACEME` call site. The only closed
`RuntimeSupervisionSetupStage` values are `WorkingDirectoryEnter` and
`TraceSetup`. Both the initial and post-`ETXTBSY` target can fail at either
stage; the registered target is reaped, retained-handle exec was not attempted,
and the errno never causes PATH fallback. The former `AnchorSetup` and
`GroupJoin` values do not exist.

A verified static native target stops at exactly one `PTRACE_EVENT_EXEC`
before its first instruction. While it is stopped, one registered observation
helper proves the stopped image's strong identity, link count, and hash match
the retained handle under kernel write denial. Missing, surplus, or abnormal
events, an unavailable checkpoint, or active-deadline expiry kills/reaps
without resume and returns no envelope.

After resume, the owner uses `PTRACE_SYSCALL` and classifies every syscall
entry before it executes. An x86_64 number with `__X32_SYSCALL_BIT` is rejected
before native-table decoding. Denied classes are:

- `process_creation`: `fork`, `vfork`, `clone`, and `clone3`;
- `image_execution`: `execve` and `execveat`;
- `executable_mapping`: native `mmap`/`mmap2`, `mprotect`,
  `pkey_mprotect`, or `shmat` requesting executable access, plus x86_64
  `uselib`;
- `executable_image_mutation`: the closed writable-image and descriptor
  acquisition operations in the product contract, including every `openat2`;
- `kernel_code_loading`: native `bpf`, `init_module`, and `finit_module`; and
- `process_signalling`: every closed signal syscall and target form.

The frozen x86_64 syscall table includes `uselib`; aarch64 does not invent an
entry for it. A denied entry never executes. The owner records
`version_probe/transitive_execution_denied`, starts exact registered-pidfd
cleanup, and emits no version. Missing, surplus, untagged, out-of-order, or
unreadable syscall stops are no-envelope `ExecutionVerificationUnavailable`.

The exact-pidfd success barrier is conjunctive:

- the verified target exited and its registered pidfd obligation was reaped;
- bounded stdout and stderr capture completed without truncation;
- the post-reap retained-handle/image identity checkpoint passed;
- the exact pidfd registry is empty.

No group scan or descendant enumeration participates. The supported claim is
only that every registered target/helper obligation is empty and no guarded
process-creation syscall executed. It is not a descendant-tree-empty claim.
Descriptor isolation prevents post-ready retention, not initial fork
inheritance, of foreign output and control descriptors. The pre-execution guard
prevents the admitted target from creating a process that could receive them.

An adversarial unrelated process in the same Unix session may churn `setpgid`
throughout the probe. It is not owner-created or registered, so its PID is
never observed for membership, never signalled, and has no effect on the
success barrier. This is the regression contract for the former anchor join
race.

Envelope-producing cleanup uses a separate fixed five-second deadline. It
signals only registered target/helper pidfds, drains or closes both pipes,
reaps every registered obligation, and empties the registry. Failures use the
closed lifecycle kinds in canonical order:

| Cleanup kind | Trigger |
| --- | --- |
| `termination_failed` | signalling a registered target/helper pidfd failed |
| `reap_failed` | a registered target/helper reap was not verified |
| `output_drain_failed` | bounded drain did not complete before read handles closed |

Observation-helper deadline, cleanup-incomplete, or protocol-invalid outcomes
remain typed no-envelope errors. In every incomplete case, the already-running
owner retains its exact registry and permit, emits an `error`, and continues
bounded identity-safe signalling/reaping. A task stuck in uninterruptible I/O
may outlive both deadlines, so no result claims termination. Caller
cancellation uses the same owner and emits no envelope; immediate Tokio runtime
shutdown cannot abandon ownership.

`RuntimeFingerprintProduceError` retains the closed
`ContainmentUnavailable`, `LaunchInputLimitExceeded`,
`OwnerResourceCapacityExceeded`, `ChildRegistrationUnavailable`,
`ChildRegistrationCleanupIncomplete`, `ObservationDeadlineExceeded`,
`ObservationCleanupIncomplete`, `ObservationProtocolInvalid`, and
`ExecutionVerificationUnavailable` families. The closed
`RuntimeOwnedChildRole` values are
`Observation(RuntimeObservationStage)`, `InitialTarget`, and
`RetryTarget`; `CapabilityCheck` is an observation stage, not a separate
child role. Observation stages do not include group membership or cleanup
membership. These producer errors never construct a partial envelope.

The fail-closed result matrix is:

| Earliest outcome | Allowed later facts |
| --- | --- |
| owner/capacity/preflight failure | typed no-envelope error before cwd or target observation |
| child registration failure | no workload ran; gated direct-child rollback reaped it or the owner retained its exact pre-registration PID obligation |
| observation or execution verification failure | typed no-envelope error; registered child is killed/reaped without fabricated facts |
| target setup failure | `working_directory_enter` or `trace_setup`; registered target reaped; no handle exec or fallback |
| post-resume exit/output/guard failure | selected identity may remain; version absent; exact-pidfd cleanup runs |
| post-reap identity change | candidate output and version discarded |
| cleanup failure | version and completed-cleanup claims absent; owner retains exact obligations |
| cancellation | no envelope; ownership survives runtime shutdown |
| success | exact target reaped, bounded streams complete, checkpoint passed, registry empty, zero exit, and one valid selected version |

The current non-Linux launcher cannot provide the same pre-observation
pidfd/ptrace contract. Windows, macOS, and other Unix return
`ContainmentUnavailable(UnsupportedPlatform)` before cwd observation or
spawn. Atomic Windows Job Object launch is outside this packet.

## Version Output Contract

Within the combined bound, stdout and stderr remain separate exact byte
sequences. Capture failures have first precedence: limit overflow or an
incomplete/read-failed stream starts cleanup and no exit or parsing failure is
substituted for it. After both streams are captured completely, termination
status has second precedence: signal termination yields only
`terminated_by_signal`, otherwise a nonzero exit yields only `nonzero_exit`.
Direct `SIGKILL` is semantic only from `AwaitEntry` or `AwaitExit`; a terminal
signal from `AwaitInitialExecExit`, internal `AwaitTermination`, or any other
illegal trace state is no-envelope `ExecutionVerificationUnavailable`.
Only a zero exit reaches UTF-8 validation, blank classification, grammar
parsing, and stream selection. Successful version evidence computes and
retains the exact selected-stream SHA-256 digest and canonicalizes the other,
validated ASCII-blank stream to the SHA-256 digest of empty bytes.

The closed runtime kind selects a whole-stream grammar. Codex Exec and Codex
JSON-RPC accept exactly `codex-cli <VERSION>`; Claude Code accepts exactly
`<VERSION> (Claude Code)`. Each permits only one optional final LF or CRLF.
`VERSION` is ASCII SemVer with exactly three numeric core components, no
invalid leading zero, and optional prerelease/build suffix. Its exact spelling
and suffix case are retained. A leading `v`/`V`, surrounding whitespace, extra
line, dependency/runtime suffix, or partial token match is rejected rather than
guessed.

`ASCII blank` is a closed byte predicate: the empty stream, or a nonempty
stream whose every byte is exactly one of `0x09` (HT), `0x0a` (LF), `0x0d`
(CR), or `0x20` (space). `0x0b` (VT), `0x0c` (FF), NUL, nonbreaking space,
and every other byte are nonblank. For a zero exit, processing order is
complete-stream UTF-8 validation, this byte predicate, then the whole-stream
product grammar. Code must not use `trim`, `split_whitespace`, or
`is_ascii_whitespace`. The selected product line still permits only its
optional final LF or CRLF; a lone final CR is legal only in an unselected blank
stream, never as the selected line ending.

Both complete streams are parsed independently before selection:

| Stdout | Stderr | Result |
| --- | --- | --- |
| matching product line | ASCII blank | select stdout |
| ASCII blank | matching product line | select stderr |
| matching product line | matching product line | `ambiguous_version`, even when the version text is equal |
| matching product line | nonblank invalid | `unparseable_version` |
| nonblank invalid | matching product line | `unparseable_version` |
| ASCII blank | ASCII blank | `empty_output` |
| any other nonblank combination | any | `unparseable_version` |

On a zero exit, invalid UTF-8 yields `invalid_utf8`; no grammar or blank
failure is also recorded. Signal and nonzero outcomes never inspect bytes for a
semantic parsing failure. On success, the payload records the selected stream,
the exact selected-stream digest, and the canonical SHA-256 digest of empty
bytes for the unselected stream after that stream has passed UTF-8 and ASCII
blank validation. This canonical blank evidence keeps strict import validation
closed without retaining raw output. Changing a product output grammar requires
a new schema grammar revision, not a heuristic first-token fallback. This
implements B-009.
