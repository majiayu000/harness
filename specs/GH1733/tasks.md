# Task Plan

## Linked Issue

GH-1733

## Spec Packet

- Product: `specs/GH1733/product.md`
- Tech: `specs/GH1733/tech.md`

## Readiness Gate

This plan is not implementation approval. GH-1733 is currently
`ready_to_spec`. Do not modify PR #1859 production code until maintainers approve
this packet and record `ready_to_implement`. Once approved, amend the original
PR branch only; do not create a replacement implementation PR or force-push.

## Implementation Tasks

- [ ] `SP1733-T1` — Owner: core fingerprint model worker. Dependencies: approved product and tech specs plus `ready_to_implement`. Covers: B-001, B-003, B-008, B-011 through B-015. Done when: the strict outer envelope carries a canonical `fingerprint_digest` separate from ASC-001 exact-source-byte component integrity; its exact domain, three `u64` frames, string escaping, raw-number preservation, two framing vectors, and one full valid vector per subject are frozen independently; closed runtime/MCP payloads, probe and lifecycle-cleanup failures, injective runtime-role and configured-server-scoped MCP tool-source derivation, exact bounded MCP text, optional presence-sensitive raw-object `annotations`, required `inputSchema`, optional presence-sensitive `outputSchema`, and raw-JSON-only duplicate-aware object-root schema parsing are implemented in the split core modules. The core manifest explicitly enables existing `serde_json/raw_value`; a borrowed `RawValue` recursive visitor preserves exact validated number lexemes without a handwritten lexer, new package, or lockfile change. The schema parser defaults absent `$schema` to Draft 2020-12, accepts only the two exact Draft 2020-12 and Draft-07 identifiers, rejects unknown/non-string/nested dialect declarations, and applies `contentSchema`, modern dependency/prefix keywords, legacy `dependencies`, tuple `items`, and `additionalItems` only under the selected dialect; Draft-07 `contentSchema` remains ordered instance data. Core owns typed `RuntimeRoleSourceBinding::derive` and strict `parse`; base source locators are bounded at 4,096 UTF-8 bytes, complete derived locators at 8,259, and raw envelopes at 2,097,152 before JSON allocation; the three closed limit reasons and precedence are frozen, and configured MCP server identity uses the exact bounded stable key. Every non-object schema root and every invalid subject/payload/source, dialect, capability, ordering, integrity, fingerprint-digest, or fixed resource-limit combination fails typed; callers cannot supply generic serializable/schema maps; constructors and parsers require an empty capability list; exact and limit-plus-one vectors cover locator/envelope sizes, number spellings, root depth, value nodes, decoded strings, direct entries, raw bytes, and canonical bytes; every core file is below 800 lines. Verify: `cargo test -p harness-core fingerprint`, `cargo test -p harness-core stack`, `cargo test -p harness-core`, `cargo check -p harness-core --all-targets`, and `cargo tree -e features -p harness-core`.
      The schema transition table additionally treats `not`, `if`, `then`,
      `else`, `contains`, `propertyNames`, and `additionalProperties` as
      object/boolean schema positions in both dialects, and Draft-07
      `additionalItems` as schema-valued even without tuple `items`; malformed
      shapes and nested `$schema` fail with closed details.
- [ ] `SP1733-T2` — Owner: runtime fingerprint worker. Dependencies: SP1733-T1. Covers: B-002 through B-010, B-014, and B-015. Done when: isolation and exact passthrough `DangerFullAccess` sandbox gates run before host observation; only Linux `x86_64`/`aarch64` with audited `close_range` descriptor isolation, `pidfd_open`, `pidfd_send_signal`, parent-child `PTRACE_O_TRACEEXEC`, `execveat(AT_EMPTY_PATH)`, strong `/proc` process/image identity, and the fixed observation protocol proceeds. The ready owner is the sole target/anchor fork, parent-side ptrace-control, wait/reap, and observation-helper-spawn owner; the target pre-exec closure's audited `PTRACE_TRACEME` is the sole exception. It atomically pidfd-registers and owns each helper or target before exposing any cancellable lease. Every cwd/candidate/boundary/hash/exec-stop/checkpoint/membership wait is bounded by the active or cleanup deadline. Observation timeout, cleanup-incomplete, and protocol-invalid are distinct closed producer errors with no envelope and exact ownership retained; no missing cwd fact or attempt outcome is fabricated. Resolution remains handle-relative and bounded with the exact open-time `ENOENT`/`ENOTDIR`, exec-time `EACCES`, and one 150 ms `ETXTBSY` retry semantics. A retained-handle exec-time `ENOENT`/`ENOTDIR` terminates interpreter authorization and deliberately does not follow the adapter's later PATH fallback; the producer never attributes a later candidate. A bounded classifier accepts only the frozen native ELF64 machine tuple, current header versions/sizes, non-extended in-file program headers, `ET_EXEC` or `ET_DYN`, and no `PT_INTERP`; scripts, dynamic/malformed/wrong-machine ELF, and non-ELF/binfmt formats fail before target creation. Supported Linux authorization requires `st_nlink == 1` initially and at pre-spawn/retry; exec-stop and post-reap revalidate link count, with multiple links failing authorization before spawn or producing identity change after target creation. Target exec uses `FD_CLOEXEC` retained-handle `execveat`, so a late script fails before interpreter execution. Every successful native exec stops at exactly one `PTRACE_EVENT_EXEC` before its first instruction and resumes only after a registered observation helper matches stopped-image strong identity plus retained-handle hash while kernel write denial is active. Changed bytes, missing/surplus events, abnormal trace state, and pre-resume timeout kill/reap without resume; verification-unavailable cases return no envelope, and no pathname fallback exists. Every `/proc` membership enumeration/revalidation uses an atomically registered observation helper with at most 64 transferred pidfds plus `more`; cleanup signals each batch individually and rescans from the beginning until only the anchor remains or the deadline expires. It never drops overflow members, uses a negative PGID, or signals the anchor before the group is empty. Membership stalls, malformed frames/helper exit, continuous churn, and anchor failures are typed. The active deadline includes observations, exec-stop, root exit, and post-reap checkpoint; probe cleanup has a separate five-second deadline. Version blank classification is exactly empty or HT/LF/CR/SP bytes after UTF-8 validation, never a generic whitespace predicate. All closed runtime/environment/command/attempt/failure contracts, executable/output bounds, Windows digests, repository authorization, exact output grammars, and prior fail-closed/no-shell requirements remain covered; no `spawn_blocking`, unbounded wait, whole-file read, unbounded pipe, warning-only fallback, or detached-`ManagedChild` completion claim remains. Verify: `cargo test -p harness-agents runtime_fingerprint`, `cargo test -p harness-agents`, and `cargo check -p harness-agents --all-targets`.
      An unavailable link count is the closed
      `target_authorization_unavailable/link_count_unprovable` reason, zero is
      `unlinked_target`, and only a count greater than one is
      `multiple_hard_links`; retry authorization records exactly one reaped
      `ETXTBSY` helper and proves no second helper or exec occurred. Exec-stop
      observation failure is no-envelope execution verification failure;
      post-reap failure is `identity/metadata_unavailable`.
      Value launch inputs are checked at 65,536 exact OS units, environment and
      setup-secret name collections/names at 1,024, and derived candidates at
      196,610 before hashing/splitting/joining or owner admission. Excluded and
      undeclared values are never read. A fail-fast global eight-owner permit,
      post-READY owner-side 67-pidfd/32-other-fd ceilings, helper-local 64
      pidfds, and child allowlists bound retained resources; the permit survives API return
      until actual owner exit. Every helper, anchor, and target first closes all
      foreign descriptors, emits a closed descriptor bootstrap status, and waits behind a
      pre-fork start gate until `pidfd_open` and registry commit succeed; closed
      rollback states prove no workload ran before `GO`.
- [ ] `SP1733-T3` — Owner: boundary contract worker. Dependencies: SP1733-T1 and SP1733-T2. Covers: B-002 and B-016. Done when: a `#[cfg(test)]`-only exhaustive workflow `RuntimeKind` mapping proves the three local kinds map one-to-one, `AnthropicApi`/`RemoteHost` are not local executables, and no non-host isolation can be interpreted as a host fingerprint subject; production call-site audit proves there is no snapshot, server, workflow-runtime, task-runner, `CodeAgent`, `AgentAdapter`, CLI, HTTP, persistence, or migration consumer; and the implementation diff matches the fourteen authorized paths exactly with no lockfile change. Verify: `cargo test -p harness-server runtime_fingerprint_runtime_kind_contract_is_exhaustive --lib` plus the manifest and `rg` audits described below.
- [ ] `SP1733-T4` — Owner: verification and handoff owner. Dependencies: SP1733-T1 through SP1733-T3. Covers: B-001 through B-016. Done when: formatting, focused/package/workspace tests, clippy, file-size/manifest/API/call-site audits, current-head independent review, Gemini, and ruleset approval all pass; every #1862 thread and valid #1859 finding is re-evaluated on the exact head. Verification must prove owner exclusivity across target/anchor fork, parent-side ptrace control, wait/reap, and helper spawn, with only the audited target pre-exec `PTRACE_TRACEME` exception; owner-atomic helper registration at every cancellation boundary; distinct typed no-envelope timeout, cleanup-incomplete, and protocol-invalid paths through post-reap and active/cleanup group-membership checks; 64-member, 65-member, larger, and continuous-churn membership behavior; no in-process blocking worker or negative-PGID signal; anchor exclusion plus typed anchor shutdown failure; static `ET_EXEC` and static-PIE success; direct/env/race-introduced shebang, `PT_INTERP`, wrong-machine, malformed-header, and non-ELF/binfmt rejection before loader/interpreter execution; missing and surplus `PTRACE_EVENT_EXEC`, abnormal trace transition, and pre-resume deadline each produce no envelope and kill/reap without resume; successful native exec reaches a verified pre-first-instruction ptrace stop and matches hash/image identity under kernel write denial before resume; macOS/other-Unix/Windows fail before cwd observation; retained cwd/target handles survive pathname replacement; repository targets never execute; and Draft 2020-12 versus Draft-07 fixtures preserve their distinct `contentSchema`, dependency, items, and extension semantics. All previous digest vectors, output/executable limits, `ETXTBSY`, cleanup, schema counting, duplicate-key, source binding, annotation, and fail-closed assertions remain unweakened; the call-site audit must prove only the owner invokes target/anchor fork, parent-side ptrace controls, and wait/reap, while only the target's audited pre-exec closure invokes `PTRACE_TRACEME`. The original PR may close GH-1733 only after all gates pass. Verify: run every command and audit in Required Verification on one current implementation head, then collect fresh PR-gate evidence.
      The exact-head matrix must additionally cover: 4,096/4,097-byte base
      locators, 8,259/8,260-byte derived locators, and 2,097,152/2,097,153-byte
      raw envelopes before allocation; raw number forms `1`, `1.0`, `1e0`,
      malformed numbers, and the long-number canonical boundary through
      `serde_json/raw_value`; exact hard-link counts 0/1/2, unavailable
      exec-stop/post-reap observations, and initial/retry/later count changes;
      all selected launch values at 65,536/65,537 units, environment/setup
      counts and names at 1,024/1,025, and derived candidates at
      196,610/196,611; eight/ninth owner admission, permit lifetime, owner
      post-READY owner 67/32, helper 64, combined 131/global 1,048 fd ceilings, foreign-fd
      isolation, and every child-role registration gate failure; every shared
      single-schema keyword plus Draft-07 unconditional `additionalItems`;
      deliberate terminal exec-time `ENOENT`/`ENOTDIR` despite
      adapter PATH continuation; and empty/HT/LF/CR/SP versus VT/FF/NUL/NBSP
      blank classification with UTF-8 precedence.

## Ownership and Ordering

Tasks run in dependency order. A later task may begin only after the dependency
task has committed a passing head. If multiple agents are used, their writable
files remain disjoint exactly as follows.

| Task | Writable files |
| --- | --- |
| SP1733-T1 | `crates/harness-core/Cargo.toml`; `crates/harness-core/src/stack/mod.rs`; `crates/harness-core/src/stack/fingerprint.rs`; `crates/harness-core/src/stack/fingerprint/model.rs`; `crates/harness-core/src/stack/fingerprint/schema.rs`; `crates/harness-core/src/stack/fingerprint/tests.rs` |
| SP1733-T2 | `crates/harness-agents/Cargo.toml`; `crates/harness-agents/src/lib.rs`; `crates/harness-agents/src/runtime_fingerprint.rs`; `crates/harness-agents/src/runtime_fingerprint/environment.rs`; `crates/harness-agents/src/runtime_fingerprint/executable.rs`; `crates/harness-agents/src/runtime_fingerprint/probe.rs`; `crates/harness-agents/src/runtime_fingerprint/tests.rs` |
| SP1733-T3 | `crates/harness-server/src/workflow_runtime_worker/runtime_profile.rs` (`#[cfg(test)]` contract only) |
| SP1733-T4 | Read-only verification, review-thread resolution, and original-branch handoff; no writable source files |

No other implementation path is authorized. The agents manifest may add only
the existing workspace `libc`; the core manifest may only enable
`serde_json/raw_value` on its existing workspace dependency. Do not edit any
other Cargo file, the lockfile,
database or persistence code, configuration schemas, adapter launch paths,
snapshots, prompts, CLI/HTTP surfaces, or high-context files. A newly
discovered need outside this manifest returns the change to spec review instead
of silently expanding scope.

## Required Verification

- [ ] Run `cargo fmt --all`.
- [ ] Run `cargo fmt --all -- --check`.
- [ ] Run `cargo check -p harness-core -p harness-agents --all-targets`.
- [ ] Run `cargo test -p harness-core fingerprint`.
- [ ] Run `cargo test -p harness-core stack`.
- [ ] Run `cargo test -p harness-core`.
- [ ] Run `cargo test -p harness-agents runtime_fingerprint`.
- [ ] Run `cargo test -p harness-agents`.
- [ ] Run
      `cargo test -p harness-server runtime_fingerprint_runtime_kind_contract_is_exhaustive --lib`.
- [ ] Run `cargo check --workspace --all-targets`.
- [ ] Run `cargo clippy --workspace --all-targets -- -D warnings`.
- [ ] Run `cargo audit`.
- [ ] Run `git diff --check`.
- [ ] Confirm every changed Rust file is below 800 lines after rustfmt.
- [ ] Confirm the implementation changed-file set equals the fourteen paths in
      the tech-spec `specrail-planned-changes` manifest.
- [ ] Confirm `Cargo.lock` is unchanged and `cargo tree -p harness-agents -i
      libc` resolves the existing pinned workspace dependency; confirm
      `cargo tree -e features -p harness-core` shows direct
      `serde_json/raw_value`.
- [ ] Use `rg` to prove the new producer APIs have no production consumer
      outside their defining modules; `#[cfg(test)]` references do not count.
- [ ] Use `rg` to prove the public MCP schema evidence API exposes no
      `from_serializable`, `serde_json::Value`, or typed-map constructor.
- [ ] Run the repository SpecRail structural checker if it exists on the
      implementation base. If it remains removed from current `main`, perform
      an independent structural review of the complete manifest and B-001
      through B-016 coverage and do not claim that a removed checker passed.
- [ ] Re-evaluate every current review thread on PR #1862 and PR #1859;
      resolve only findings demonstrably addressed on the current head and post
      no comment or reply without separate authorization.
- [ ] Wait for current-head CI, independent native review, Gemini review, and
      repository ruleset approval before merge.
- [ ] Obtain mandatory human security review of the effective-sandbox
      fail-closed gate; Linux observation-process fixed-frame/`SCM_RIGHTS`
      protocol; bounded launch/environment/setup-secret counting; eight-owner
      admission and permit lifetime; owner/helper/child fd ledgers; allocation-
      free post-fork work; foreign-fd isolation before `DESCRIPTORS_READY`;
      pre-fork start gates and direct-child rollback; pidfd registration,
      revalidation, handoff, and reap ownership; retained cwd/target descriptors
      and `fchdir`; final-target authorization; `FD_CLOEXEC` shebang rejection;
      ptrace exec-stop/first-instruction ordering and hash/image validation under
      kernel write denial; retained-handle pre-exec; non-anchor-only signalling
      and typed anchor exit/termination/reap ordering;
      argument/environment pointer ownership; NUL validation; stage-tagged
      errno propagation; released-PGID exclusion; and proof that `ENOEXEC`
      cannot invoke a shell.

## Handoff Notes

- PR #1859 remains the sole implementation PR and must be repaired on its
  original branch after the readiness gate opens.
- `runner_observed` describes evidence strength and trust; it never replaces
  repository, user-global, admin, system, runtime, or genuine runner ownership.
- Fingerprint bindings accept base source locators through 4,096 UTF-8 bytes
  and complete derived locators through 8,259 bytes, with checked arithmetic
  before allocation. Strict envelope input is capped at 2,097,152 raw bytes
  before JSON decoding. The three closed limit reasons and parser/constructor
  precedence are frozen. These limits do not change global ASC-001 validity.
- Runtime launch inputs are bounded before digest/split/join and before owner
  admission: 65,536 exact Unix-byte or Windows-UTF-16 units per selected value,
  1,024 observation environment entries/keys and setup-secret names/name units,
  and 196,610 checked derived-candidate units. Excluded or undeclared values are
  never read. Limit failure is no-envelope and cannot allocate an fd, occupy an
  owner slot, or select another PATH candidate.
- At most eight fingerprint-specific owners exist concurrently. Each retains
  after `DESCRIPTORS_READY` at most 67 owner-side pidfds and 32 other fds; a
  membership helper has at most 64 pidfds, making 131 retained references per
  descriptor-isolated fingerprint and 1,048 across eight after READY. Pre-READY
  inheritance has a deadline/rollback time bound but no claimed numeric
  reference bound. One membership batch is handled at a time, and the permit is
  held through every retained obligation until actual thread exit. Every owned
  child closes all descriptors outside its fixed role allowlist and reports
  `descriptors_ready`, `descriptor_isolation_unavailable`, or
  `descriptor_isolation_failed`; it cannot run workload until pidfd registry
  commit.
  Bootstrap unavailability maps only to containment after reap, while a later
  role's isolation failure maps only to child registration. Deadline is used
  only before a concrete status. Rollback uses only the still-unreaped direct
  child identity and never a negative PGID.
- A bare configured command follows the frozen Unix or Windows v0.1 launch
  contract independently of the build compiler. Unix rejects unset PATH rather
  than guessing a default, attempts inspected candidates in PATH order, and
  advances only after exact retained-handle `EACCES`; Windows selects one absolute
  candidate. Neither path authorizes `PATHEXT` inference, guessed relative
  bases, another basename, an `ENOEXEC` shell, `which`, or a package manager.
  The closed command form makes bare search skips distinct from
  absolute/qualified failures: exact open `ENOENT`/`ENOTDIR` is absent, while
  an existing non-regular or mode-ineligible configured path retains its
  identity failure. Fingerprint selection parity applies only to an eligible
  native target that passes authorization and executes. Retained-handle
  exec-time `ENOENT`/`ENOTDIR` is a deliberate terminal security divergence:
  the producer does not execute or attribute a later PATH candidate even if
  the adapter would continue.
- `codex.cloud.setup_secret_env` is an unconditional bounded exclusion set.
  Setup values and undeclared values are never read, copied, bounded, hashed, or
  passed to the version child. Only exclusion-surviving closed-policy values
  receive the 65,536-unit value limit.
- The child `PATH` portion of resolution context is exactly the sanitized value
  given to the probe. Windows current-executable, system, Windows-directory,
  and parent-PATH search inputs are explicit optional resolution-only facts,
  represented by four exact-domain UTF-16LE digests with absence distinct from
  present empty. The closed runtime-kind table admits no other version-child
  value. Configured working-directory spelling and Unix directory-handle
  identity have separate fixed digests; the parent retains that handle and
  every qualified/relative-PATH observation, open, and checkpoint uses
  `fstatat`/`openat` against it, while every target helper enters it with
  stage-tagged `fchdir` before handle exec. Replacing the cwd pathname cannot
  redirect either side.
- Executable size, metadata, and digest come from one retained opened
  regular-file handle. Supported Linux uses device/inode and mode bits; Windows
  requires volume serial plus file ID but records `containment_unavailable`
  before cwd observation or spawn. All potentially blocking cwd, open,
  classification, hash, and checkpoint operations run in registered,
  kill-isolated observation subprocesses under the active deadline, never
  `spawn_blocking`. The owner creates and pidfd-registers each helper before
  exposing a lease; timeout returns a typed producer error with no envelope
  while the owner retains an exact unreaped pidfd and no termination claim is
  fabricated. The fixed executable
  ceiling is 67,108,864 bytes, and byte 67,108,865 fails typed. Retained-handle
  size/digest and path strong identity are checked initially, immediately
  before spawn, at the pre-first-instruction exec stop under kernel write
  denial, and after reap; a
  mismatch discards version evidence. Supported Linux also requires
  `st_nlink == 1` at authorization and rechecks it before spawn, after
  `ETXTBSY`, at exec-stop, and after reap. An unavailable count is
  `link_count_unprovable`, zero is `unlinked_target`, and only a count greater
  than one is `multiple_hard_links`. Multiple links
  fail authorization before spawn, fail retry authorization after one reaped
  `ETXTBSY` helper and before a second helper, or become identity change after
  target creation. Exec-stop observation failure is no-envelope execution
  verification failure; post-reap failure is `identity/metadata_unavailable`.
  This closes hard-link ambiguity but does not claim
  bind-mount alias exclusion. This checkpoint correlation is not
  path-history attestation. A bounded classifier accepts only
  current-architecture static ELF without `PT_INTERP`; exact leading `#!`,
  dynamic/malformed ELF, wrong-architecture ELF, and every non-ELF/binfmt
  format fail
  `interpreter_authorization_unavailable` before any interpreter, anchor, or
  target; a late shebang is blocked by `FD_CLOEXEC` `execveat`. Supported native
  Linux binaries stop at `PTRACE_EVENT_EXEC` before their first instruction and
  resume only after handle hash and image identity match; the pathname is never
  reopened. A supported Linux platform without such a primitive emits
  `handle_execution_unavailable` before creating an anchor or target and never
  fall back to a path; Windows independently returns
  `containment_unavailable` first under its frozen platform matrix.
- Runtime v0.1 is host-only. Container and microVM inputs fail before host
  resolution, file access, or process creation. It also accepts only the
  adapter's exact passthrough `DangerFullAccess` state with no allowed-write
  narrowing; every restricted sandbox state fails
  `sandbox_parity_unavailable` at the same pre-observation gate.
- Repository-owned runtime configuration and any opened target inside a
  validated repository/worktree boundary are never run for observation; their
  identity/hash may be retained with a closed `probe_not_authorized` reason,
  unavailable target authorization also prevents spawn, and neither policy has
  a caller override.
- Runtime environment evidence comes only from the exact closed runtime-kind
  table. PATH and `CLAUDE_CONFIG_DIR` use separate frozen digest domains plus
  raw Unix bytes or Windows UTF-16LE units; PATH is the sole version-child key.
  Platform-normalized setup-secret exclusion runs first, and Windows rejects
  canonical key collisions or non-ASCII ambiguity.
- Version stream processing checks the output cap, validates complete UTF-8,
  then classifies blank as exactly empty or bytes from HT/LF/CR/SP. VT, FF,
  NUL, NBSP, and every other byte are nonblank; no generic trimming or
  whitespace predicate is allowed.
- The combined stdout/stderr cap is caller-selectable only in `1..=65_536`,
  validated before allocation or helper creation, inclusive, and bounded at
  read time. After owner readiness, the five-second active deadline begins
  before cwd observation and includes every observation plus the post-reap
  checkpoint; cleanup has a separate five-second deadline. Linux claims only
  root/original-process-group supervision, not non-escapable descendant
  containment. Cleanup enumerates and revalidates exact non-anchor pidfds and
  signals each individually; it never uses a negative PGID, and the anchor
  exits only after the group is proven empty. Any helper/root/member setup,
  termination, drain, reap, or verification failure is typed and leaves
  ownership with the pre-existing owner. Cancellation uses the same owner
  without evidence; macOS, other Unix, and Windows record
  `containment_unavailable` before cwd observation.
- MCP server identity is derived from a typed exact stable configuration-entry
  key of at most 1,024 UTF-8 bytes rather than an arbitrary component. MCP
  descriptions remain exact. Optional annotations accept only bounded,
  duplicate-aware raw JSON objects; standard hints, title/vendor values, and
  ordered arrays enter the fingerprint without inferring capabilities.
  Required `inputSchema` and optional
  presence-sensitive `outputSchema` share the same duplicate-aware bounded
  contract. Non-object schema roots fail before canonicalization. Absent
  `$schema` means Draft 2020-12; only the exact Draft 2020-12 and Draft-07
  identifiers are accepted, and unknown/non-string/nested declarations fail
  typed. Keyword context follows that selected dialect: modern
  `contentSchema`, `dependentRequired`, `$defs`, and prefix semantics apply only
  to Draft 2020-12; legacy `definitions`, `dependencies`, and tuple `items`
  semantics apply only to Draft-07. The shared
  `not`/conditional/contains/property/additional-property positions are
  schema-valued in both dialects, and Draft-07 `additionalItems` is always
  schema-valued. Malformed values fail closed. Draft-07
  `contentSchema` remains ordered instance data. Annotation, extension, and
  ordered-schema arrays retain order; evidence accepts only duplicate-aware raw
  JSON. `harness-core` explicitly enables existing `serde_json/raw_value`;
  borrowed recursive `RawValue` slices preserve exact validated number tokens
  without a handwritten lexer, new package, or lockfile change. Fixed
  text/annotation/schema limits fail typed under the frozen
  depth, node, decoded-string, direct-entry, raw-byte, and canonical-byte
  counting rules.
- Envelope `fingerprint_digest` uses the frozen domain, three length frames,
  canonical payload encoding, raw number-token preservation, and independent
  vectors for both subjects; it covers subject plus canonical payload.
  ASC-001 component integrity remains exact-source-byte evidence or absence.
- This issue delivers producer APIs only. ASC-005 owns snapshot consumption and
  ASC-026 owns native snapshot/diff commands.
