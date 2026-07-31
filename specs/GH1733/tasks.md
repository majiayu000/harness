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

- [ ] `SP1733-T1` — Owner: core fingerprint model worker. Dependencies: approved product and tech specs plus `ready_to_implement`. Covers: B-001, B-003, B-008, B-011 through B-015. Done when: the strict outer envelope carries a canonical `fingerprint_digest` separate from ASC-001 exact-source-byte component integrity; closed runtime/MCP payloads, probe and lifecycle-cleanup failures, injective server-scoped MCP tool-source derivation, exact bounded MCP text, raw-JSON-only duplicate-aware schema parsing, object/boolean/legacy-array `items` context, and context-aware canonicalization are implemented in the split core modules; every fixed tool/schema resource limit is enforced at exact and limit-plus-one boundaries without panic or unbounded allocation; callers cannot supply pre-encoded tool sources or generic serializable/schema maps; constructors emit and parsers require an empty component capability list; invalid subject/payload/source, capability, schema, ordering, integrity, fingerprint-digest, and resource-limit combinations fail typed; every core file is below 800 lines. Verify: `cargo test -p harness-core fingerprint`, `cargo test -p harness-core stack`, `cargo test -p harness-core`, and `cargo check -p harness-core --all-targets`.
- [ ] `SP1733-T2` — Owner: runtime fingerprint worker. Dependencies: SP1733-T1. Covers: B-002 through B-010, B-014, and B-015. Done when: the runtime producer accepts only the three closed local executable kinds, exhaustively consumes the existing `IsolationTier` with host as the only supported v0.1 tier, and enforces their exact whole-output grammars; container and microVM inputs fail before host PATH/file/process observation; validated ownership is preserved; repository sources produce identity-only `probe_not_authorized` evidence and cannot spawn or be caller-promoted, while the other five source scopes follow the closed eligibility policy; one command is resolved with the explicit Unix/pinned-Rust-Windows launch matrix and only the selected absolute path is spawned; the runtime-kind environment table is private and exhaustive, arbitrary/cross-runtime keys cannot be declared or exposed, setup secrets override it, and Windows canonical key comparison rejects collisions and non-ASCII ambiguity; one retained handle is hashed initially, immediately before spawn, and after reap with strong path identity checks; Unix supervision claims only root/original-group evidence, never non-escapable descendant containment; every explicit cleanup operation and the shared five-second deadline has typed failure injection, closes stuck read handles, transfers ownership before returning incomplete evidence, and leaves the runtime-independent owner running; cancellation transfers ownership and emits no evidence; Windows emits `containment_unavailable` before spawn; the combined output cap is inclusive and detects only byte max-plus-one; both output streams are parsed before selection; and every incomplete observation is represented without a version or fabricated cleanup claim. No duplicate isolation enum, arbitrary runtime string, caller environment classification, repository-code execution, descendant-tree-empty claim, token-scan/first-token parser, `PATHEXT` assumption, guessed relative base, weak Windows identity, root-only `kill_on_drop` or existing detached `ManagedChild` completion claim, shell, `which`, whole-file read, `Command::output`, unbounded pipe, heuristic secret classification, or warning-only fallback remains. Verify: `cargo test -p harness-agents runtime_fingerprint`, `cargo test -p harness-agents`, and `cargo check -p harness-agents --all-targets`.
- [ ] `SP1733-T3` — Owner: boundary contract worker. Dependencies: SP1733-T1 and SP1733-T2. Covers: B-002 and B-016. Done when: a `#[cfg(test)]`-only exhaustive workflow `RuntimeKind` mapping proves the three local kinds map one-to-one, `AnthropicApi`/`RemoteHost` are not local executables, and no non-host isolation can be interpreted as a host fingerprint subject; production call-site audit proves there is no snapshot, server, workflow-runtime, task-runner, `CodeAgent`, `AgentAdapter`, CLI, HTTP, persistence, or migration consumer; and the implementation diff matches the twelve authorized paths exactly. Verify: `cargo test -p harness-server runtime_fingerprint_runtime_kind_contract_is_exhaustive --lib` plus the manifest and `rg` audits described below.
- [ ] `SP1733-T4` — Owner: verification and handoff owner. Dependencies: SP1733-T1 through SP1733-T3. Covers: B-001 through B-016. Done when: formatting, focused suites, both package suites, workspace check, workspace clippy, file-size limits, changed-file manifest, public-constructor and production-call-site audits, independent review, every current PR #1862 thread and every valid PR #1859 finding are re-evaluated on the current head, parser tests do not claim source-byte provenance, no test claims process groups are non-escapable containment, repository marker programs never execute, the existing detached `ManagedChild` reaper is not claimed as cancellation-complete evidence, every cleanup failure and resource-limit boundary is covered without weakened assertions, and current-head CI, Gemini review, and repository ruleset approval all pass; the original PR may close GH-1733 only after all gates pass. Verify: run every command and audit in Required Verification on one current implementation head, then collect fresh PR-gate evidence.

## Ownership and Ordering

Tasks run in dependency order. A later task may begin only after the dependency
task has committed a passing head. If multiple agents are used, their writable
files remain disjoint exactly as follows.

| Task | Writable files |
| --- | --- |
| SP1733-T1 | `crates/harness-core/src/stack/mod.rs`; `crates/harness-core/src/stack/fingerprint.rs`; `crates/harness-core/src/stack/fingerprint/model.rs`; `crates/harness-core/src/stack/fingerprint/schema.rs`; `crates/harness-core/src/stack/fingerprint/tests.rs` |
| SP1733-T2 | `crates/harness-agents/src/lib.rs`; `crates/harness-agents/src/runtime_fingerprint.rs`; `crates/harness-agents/src/runtime_fingerprint/environment.rs`; `crates/harness-agents/src/runtime_fingerprint/executable.rs`; `crates/harness-agents/src/runtime_fingerprint/probe.rs`; `crates/harness-agents/src/runtime_fingerprint/tests.rs` |
| SP1733-T3 | `crates/harness-server/src/workflow_runtime_worker/runtime_profile.rs` (`#[cfg(test)]` contract only) |
| SP1733-T4 | Read-only verification, review-thread resolution, and original-branch handoff; no writable source files |

No other implementation path is authorized. In particular, do not edit Cargo
files, the lockfile, database or persistence code, configuration schemas,
adapter launch paths, snapshots, prompts, CLI/HTTP surfaces, or high-context
files. A newly discovered need outside this manifest returns the change to spec
review instead of silently expanding scope.

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
- [ ] Run `git diff --check`.
- [ ] Confirm every changed Rust file is below 800 lines after rustfmt.
- [ ] Confirm the implementation changed-file set equals the twelve paths in
      the tech-spec `specrail-planned-changes` manifest.
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

## Handoff Notes

- PR #1859 remains the sole implementation PR and must be repaired on its
  original branch after the readiness gate opens.
- `runner_observed` describes evidence strength and trust; it never replaces
  repository, user-global, admin, system, runtime, or genuine runner ownership.
- A bare configured command resolves only the first candidate selected by the
  explicit Unix or pinned-Rust-Windows launch contract, then spawns its absolute
  path. It does not authorize `PATHEXT` inference, guessed relative bases, PATH
  scanning beyond that basename, candidate execution, a shell, `which`, or a
  package manager.
- `codex.cloud.setup_secret_env` is an unconditional exclusion set. Setup
  values never enter evidence or the version child, regardless of key spelling.
- The child `PATH` portion of resolution context is exactly the sanitized value
  given to the probe. Windows current-executable, system, Windows-directory,
  and parent-PATH search inputs are explicit resolution-only facts. The closed
  runtime-kind table admits no other version-child value.
- Executable size, metadata, and digest come from one retained opened
  regular-file handle. Unix uses device/inode and mode bits; Windows requires
  volume serial plus file ID, then v0.1 records `containment_unavailable`
  without spawning or claiming loadability. On platforms with supervised spawn,
  load failure is `spawn_failed`. Retained-handle
  size/digest and path strong identity are checked initially, immediately
  before spawn, and after reap; a
  mismatch discards version evidence. This checkpoint correlation is not
  executed-byte attestation.
- Runtime v0.1 is host-only. Container and microVM inputs fail before host
  resolution, file access, or process creation.
- Repository-owned runtime executables are never run for observation; their
  identity/hash may be retained with `probe_not_authorized`, and the source
  policy has no caller override.
- Runtime environment evidence comes only from the exact closed runtime-kind
  table. PATH is the sole version-child key; platform-normalized setup-secret
  exclusion runs first, and Windows rejects canonical key collisions or
  non-ASCII ambiguity.
- The combined stdout/stderr cap is inclusive and bounded at read time.
  Unix claims root/original-process-group supervision only, not non-escapable
  descendant containment. Explicit cleanup uses a fixed five-second deadline;
  any termination, drain, reap, or verification failure is typed and transfers
  ownership before returning. Cancellation uses the same runtime-independent
  owner without emitting evidence; Windows v0.1 records
  `containment_unavailable` before spawn.
- MCP descriptions remain exact. Schema arrays are reordered only at the six
  approved schema-set locations; annotation, extension, and ordered-schema
  arrays retain order. Evidence construction accepts only duplicate-aware raw
  JSON, not generic serializable values. Object/boolean `items` remains schema
  context, legacy array `items` remains ordered, and fixed text/schema limits
  fail typed before unbounded work.
- Envelope `fingerprint_digest` covers subject plus canonical payload;
  ASC-001 component integrity remains exact-source-byte evidence or absence.
- This issue delivers producer APIs only. ASC-005 owns snapshot consumption and
  ASC-026 owns native snapshot/diff commands.
