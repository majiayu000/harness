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

- [ ] `SP1733-T1` — Owner: core fingerprint model worker. Dependencies: approved product and tech specs plus `ready_to_implement`. Covers: B-001, B-003, B-008, B-011 through B-015. Done when: the strict outer envelope carries a canonical `fingerprint_digest` separate from ASC-001 exact-source-byte component integrity; its exact domain, three `u64` frames, string escaping, raw-number preservation, two framing vectors, and one full valid vector per subject are frozen independently; closed runtime/MCP payloads, probe and lifecycle-cleanup failures, injective runtime-role and configured-server-scoped MCP tool-source derivation, exact bounded MCP text, required `inputSchema`, optional presence-sensitive `outputSchema`, raw-JSON-only duplicate-aware object-root schema parsing, object/boolean/legacy-array nested `items`, legacy `additionalItems`, `contentSchema` schema context, `dependentRequired` string-set context, and context-aware canonicalization are implemented in the split core modules; core owns typed `RuntimeRoleSourceBinding::derive` and strict `parse`, the three runtime roles derived from one base have distinct IDs while preserving base scope and exact-source integrity or absence, callers cannot pre-encode a role, and missing, malformed, noncanonical, or payload-wrong role suffixes fail typed; a typed configured MCP server binding derives identity from the exact stable persisted-entry key, enforces the inclusive 1,024-byte limit before hex expansion, accepts no arbitrary prebuilt server component, and top-level envelope parsing strictly re-derives both server and tool suffixes then matches the payload server ID and tool name; every non-object MCP schema root fails typed before canonicalization; every fixed tool/schema resource limit is enforced at exact and limit-plus-one boundaries without panic or unbounded allocation; callers cannot supply generic serializable/schema maps; constructors emit and parsers require an empty component capability list; invalid subject/payload/source, capability, schema, ordering, integrity, fingerprint-digest, and resource-limit combinations fail typed; every core file is below 800 lines. Verify: `cargo test -p harness-core fingerprint`, `cargo test -p harness-core stack`, `cargo test -p harness-core`, and `cargo check -p harness-core --all-targets`.
- [ ] `SP1733-T2` — Owner: runtime fingerprint worker. Dependencies: SP1733-T1. Covers: B-002 through B-010, B-014, and B-015. Done when: the runtime producer accepts only the three closed local executable kinds, consumes the core runtime-role binding rather than deriving locators independently, exhaustively consumes the existing `IsolationTier` with host as the only supported v0.1 tier, and enforces their exact whole-output grammars; container and microVM inputs fail before host PATH/file/process observation; validated ownership is preserved; repository configuration and any opened target inside a validated repository/worktree boundary produce identity-only `probe_not_authorized` evidence, unavailable target classification fails closed, and no caller can promote either result; every payload includes the exact platform-specific configured-command digest without raw command text. Unix bare-name resolution rejects unset PATH rather than guessing a platform default, while absolute and qualified commands remain valid. After final authorization, Unix uses direct no-shell retained-handle exec with exact configured-command `argv[0]`, never reopens or executes the candidate pathname, and supervised Unix platforms without a no-path primitive emit terminal `handle_execution_unavailable` before anchor or target creation; Windows instead returns `containment_unavailable` first under the frozen platform matrix. Exact target-handle `EACCES` alone advances a bare-name search. One exact `ETXTBSY` waits 150 milliseconds, repeats target authorization/hash/path identity, and retries that retained handle only when the checkpoint remains authorized and stable. Changed authorization is `retry_not_authorized` or `retry_authorization_unavailable`; second `ETXTBSY`, `ENOEXEC`, and other errors are terminal; initial or post-`ETXTBSY` `supervision_setup_failed` helpers are reaped before the affected handle exec. `open_failed` stops resolution, every candidate has a closed exec sequence/outcome, unavailable initial target classification is terminal `authorization_unavailable` paired exactly with `target_authorization_unavailable`, bare-name evidence is limited to 64 entries, and absolute/qualified paths never search-fallback; candidate 65 fails typed before observation. Windows uses the compiler-independent frozen v0.1 search algorithm, with a differential adapter-conformance test that fails on drift, and carries four optional current-executable-directory, system-directory, Windows-directory, and parent-PATH context digests with exact domains, absent/present states, UTF-16LE framing, and independent fixed vectors. The runtime-kind environment table is private and exhaustive; exact PATH and `CLAUDE_CONFIG_DIR` digests use frozen domains plus platform-native OS-unit encoding without raw values; arbitrary/cross-runtime keys cannot be declared or exposed; setup secrets override it; and Windows canonical key comparison rejects collisions and non-ASCII ambiguity. Unix uses nonblocking close-on-exec open plus authoritative handle metadata for the initial open and both path checkpoints so FIFOs and other special files cannot block or reach hashing. One retained regular-file handle is hashed initially, immediately before spawn, and after reap with strong path identity checks. The fixed, non-caller-adjustable executable ceiling accepts 67,108,864 bytes and rejects byte 67,108,865 before unbounded work. Authorization-race tests replace the pathname after the final checkpoint and prove only the retained handle can execute. Unix reserves and handshakes a runtime-independent owner before any helper under a fixed one-second bounded stop/join contract, keeps a live group anchor through every negative-PGID signal and membership decision, proves only the anchor remains before reaping it, and never uses the released PGID. Supervision claims only root/original-group evidence, never non-escapable descendant containment. The active probe has a fixed five-second deadline starting before anchor creation and includes every target attempt and the `ETXTBSY` delay. Every terminal path reaps the root and proves no non-anchor member remains before success; a lingering same-group child suppresses version success and starts cleanup. Every explicit cleanup operation and the separate five-second cleanup deadline has typed failure injection, closes stuck read handles, leaves ownership with the already-running owner before returning incomplete evidence, and has no on-demand thread or unbounded drop fallback. Cancellation activates that owner and emits no evidence; Windows emits `containment_unavailable` before spawn. `max_output_bytes` is validated in `1..=65_536` before allocation/helper creation and the inclusive collector detects only byte max-plus-one; both output streams are parsed before selection; and every incomplete observation is represented without a version, raw OS diagnostic, or fabricated cleanup claim. No duplicate isolation enum, arbitrary runtime string, caller environment classification, repository-code execution, unresolved target authorization, authorization-to-exec pathname race, blocking special-file open, path-based post-authorization exec, `execvp`/`ENOEXEC` shell fallback, broad or repeated spawn retry, compiler-derived Windows resolver, same-group child leak, released-PGID signal, descendant-tree-empty claim, token-scan/first-token parser, `PATHEXT` assumption, guessed relative base, weak Windows identity, root-only `kill_on_drop` or existing detached `ManagedChild` completion claim, shell, `which`, whole-file read, `Command::output`, unbounded pipe, heuristic secret classification, or warning-only fallback remains. Verify: `cargo test -p harness-agents runtime_fingerprint`, `cargo test -p harness-agents`, and `cargo check -p harness-agents --all-targets`.
- [ ] `SP1733-T3` — Owner: boundary contract worker. Dependencies: SP1733-T1 and SP1733-T2. Covers: B-002 and B-016. Done when: a `#[cfg(test)]`-only exhaustive workflow `RuntimeKind` mapping proves the three local kinds map one-to-one, `AnthropicApi`/`RemoteHost` are not local executables, and no non-host isolation can be interpreted as a host fingerprint subject; production call-site audit proves there is no snapshot, server, workflow-runtime, task-runner, `CodeAgent`, `AgentAdapter`, CLI, HTTP, persistence, or migration consumer; and the implementation diff matches the thirteen authorized paths exactly with no lockfile change. Verify: `cargo test -p harness-server runtime_fingerprint_runtime_kind_contract_is_exhaustive --lib` plus the manifest and `rg` audits described below.
- [ ] `SP1733-T4` — Owner: verification and handoff owner. Dependencies: SP1733-T1 through SP1733-T3. Covers: B-001 through B-016. Done when: formatting, focused suites, both package suites, workspace check, workspace clippy, file-size limits, changed-file manifest, public-constructor and production-call-site audits, independent review, every current PR #1862 thread and every valid PR #1859 finding are re-evaluated on the current head, parser tests do not claim source-byte provenance, accept mismatched MCP source bindings, omit advertised output schema, or accept non-object MCP roots, all fingerprint/environment/command/candidate digests use independent fixed vectors, no test claims process groups are non-escapable containment or uses a released PGID, repository-source and repository-target marker programs never execute, initial/checkpoint special-file opens cannot hang, zero-exit same-group children cannot yield success, owner reservation and setup failure occur before target exec, the existing detached `ManagedChild` reaper is not claimed as cancellation-complete evidence, the exact bounded `ETXTBSY` retry, active/cleanup deadlines, output-cap range, stable-key bound, and every cleanup/schema/resource-limit boundary are covered without weakened assertions, and current-head CI, Gemini review, and repository ruleset approval all pass; the original PR may close GH-1733 only after all gates pass. Verify: run every command and audit in Required Verification on one current implementation head, then collect fresh PR-gate evidence.

## Ownership and Ordering

Tasks run in dependency order. A later task may begin only after the dependency
task has committed a passing head. If multiple agents are used, their writable
files remain disjoint exactly as follows.

| Task | Writable files |
| --- | --- |
| SP1733-T1 | `crates/harness-core/src/stack/mod.rs`; `crates/harness-core/src/stack/fingerprint.rs`; `crates/harness-core/src/stack/fingerprint/model.rs`; `crates/harness-core/src/stack/fingerprint/schema.rs`; `crates/harness-core/src/stack/fingerprint/tests.rs` |
| SP1733-T2 | `crates/harness-agents/Cargo.toml`; `crates/harness-agents/src/lib.rs`; `crates/harness-agents/src/runtime_fingerprint.rs`; `crates/harness-agents/src/runtime_fingerprint/environment.rs`; `crates/harness-agents/src/runtime_fingerprint/executable.rs`; `crates/harness-agents/src/runtime_fingerprint/probe.rs`; `crates/harness-agents/src/runtime_fingerprint/tests.rs` |
| SP1733-T3 | `crates/harness-server/src/workflow_runtime_worker/runtime_profile.rs` (`#[cfg(test)]` contract only) |
| SP1733-T4 | Read-only verification, review-thread resolution, and original-branch handoff; no writable source files |

No other implementation path is authorized. The agents manifest may add only
the existing workspace `libc`; do not edit any other Cargo file, the lockfile,
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
- [ ] Confirm the implementation changed-file set equals the thirteen paths in
      the tech-spec `specrail-planned-changes` manifest.
- [ ] Confirm `Cargo.lock` is unchanged and `cargo tree -p harness-agents -i
      libc` resolves the existing pinned workspace dependency.
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
- [ ] Obtain mandatory human security review of Unix nonblocking target open,
      final-target repository-boundary authorization, the retained-handle
      pre-exec path, pre-reserved cleanup ownership, anchored process-group membership
      proof, and post-root cleanup, including argument/environment pointer
      ownership, NUL validation, stage-tagged errno propagation, released-PGID
      exclusion, and proof that `ENOEXEC` cannot invoke a shell.

## Handoff Notes

- PR #1859 remains the sole implementation PR and must be repaired on its
  original branch after the readiness gate opens.
- `runner_observed` describes evidence strength and trust; it never replaces
  repository, user-global, admin, system, runtime, or genuine runner ownership.
- A bare configured command follows the frozen Unix or Windows v0.1 launch
  contract independently of the build compiler. Unix rejects unset PATH rather
  than guessing a default, attempts inspected candidates in PATH order, and
  advances only after exact retained-handle `EACCES`; Windows selects one absolute
  candidate. Neither path authorizes `PATHEXT` inference, guessed relative
  bases, another basename, an `ENOEXEC` shell, `which`, or a package manager.
- `codex.cloud.setup_secret_env` is an unconditional exclusion set. Setup
  values never enter evidence or the version child, regardless of key spelling.
- The child `PATH` portion of resolution context is exactly the sanitized value
  given to the probe. Windows current-executable, system, Windows-directory,
  and parent-PATH search inputs are explicit optional resolution-only facts,
  represented by four exact-domain UTF-16LE digests with absence distinct from
  present empty. The closed runtime-kind table admits no other version-child
  value.
- Executable size, metadata, and digest come from one retained opened
  regular-file handle. Unix uses device/inode and mode bits; Windows requires
  volume serial plus file ID, then v0.1 records `containment_unavailable`
  without spawning or claiming loadability. On platforms with supervised spawn,
  load failure is `spawn_failed`. The fixed executable ceiling is 67,108,864
  bytes, and byte 67,108,865 fails typed. Retained-handle
  size/digest and path strong identity are checked initially, immediately
  before spawn, and after reap; a
  mismatch discards version evidence. This checkpoint correlation is not
  executed-byte attestation. Unix executes only the retained authorized handle
  after the final checkpoint; it never reopens the pathname for execution.
  Supervised Unix platforms without such a primitive emit
  `handle_execution_unavailable` before creating an anchor or target and never
  fall back to a path; Windows independently returns
  `containment_unavailable` first under its frozen platform matrix.
- Runtime v0.1 is host-only. Container and microVM inputs fail before host
  resolution, file access, or process creation.
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
- The combined stdout/stderr cap is caller-selectable only in `1..=65_536`,
  validated before allocation or helper creation, inclusive, and bounded at
  read time. The separate active and cleanup deadlines are each exactly five
  seconds.
  Unix claims root/original-process-group supervision only, not non-escapable
  descendant containment. A runtime-independent owner and process-group anchor
  exist before target creation; every PGID operation occurs while the anchor is
  live. Every terminal path reaps the root and proves only the anchor remains
  before success, then reaps the anchor and never reuses that PGID; a lingering
  same-group child suppresses version and starts cleanup. Any setup,
  termination, drain, reap, or verification failure is typed and leaves
  ownership with the pre-existing owner before returning. Cancellation uses the
  same owner without emitting evidence; Windows v0.1 records
  `containment_unavailable` before spawn.
- MCP server identity is derived from a typed exact stable configuration-entry
  key of at most 1,024 UTF-8 bytes rather than an arbitrary component. MCP
  descriptions remain exact. Required `inputSchema` and optional
  presence-sensitive `outputSchema` share the same duplicate-aware bounded
  contract. Non-object schema roots fail before canonicalization. Schema arrays are
  reordered only at the seven approved schema-set keyword families;
  annotation, extension, and ordered-schema arrays retain order. Evidence
  construction accepts only duplicate-aware raw JSON, not generic serializable
  values. Object/boolean `items` remain schema context; legacy array `items`
  remains ordered and makes sibling `additionalItems` schema context, while
  missing/object/boolean `items` keeps `additionalItems` in instance context.
  `dependentRequired` property arrays are string sets, and fixed text/schema
  limits fail typed before unbounded work. `contentSchema` always enters schema
  context, including nested `required` and `oneOf`.
- Envelope `fingerprint_digest` uses the frozen domain, three length frames,
  canonical payload encoding, raw number-token preservation, and independent
  vectors for both subjects; it covers subject plus canonical payload.
  ASC-001 component integrity remains exact-source-byte evidence or absence.
- This issue delivers producer APIs only. ASC-005 owns snapshot consumption and
  ASC-026 owns native snapshot/diff commands.
