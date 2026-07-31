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

- [ ] `SP1733-T1` — Owner: core fingerprint model worker. Dependencies: approved product and tech specs plus `ready_to_implement`. Covers: B-001, B-003, B-008, B-011 through B-015. Done when: the strict outer envelope, closed runtime/MCP payloads, typed failure vocabulary, source-preserving component construction, exact MCP text, duplicate-aware schema parsing, context-aware canonicalization, and canonical payload digests are implemented in the split core modules; constructors emit and parsers require an empty component capability list; invalid subject/payload/source, capability, schema, ordering, and integrity combinations fail typed; every core file is below 800 lines. Verify: `cargo test -p harness-core fingerprint`, `cargo test -p harness-core stack`, `cargo test -p harness-core`, and `cargo check -p harness-core --all-targets`.
- [ ] `SP1733-T2` — Owner: runtime fingerprint worker. Dependencies: SP1733-T1. Covers: B-002 through B-010, B-014, and B-015. Done when: the runtime producer accepts only the three closed local executable kinds; preserves validated ownership; resolves the one configured command with native launch parity; excludes all setup secrets; records only typed declared environment evidence; hashes one opened handle off the async worker; detects path identity changes; supervises, bounds, terminates, and reaps probes; parses exactly one v0.1 version token; and emits a typed failure for every incomplete observation. No arbitrary runtime string, shell, `which`, whole-file read, `Command::output`, unbounded pipe, heuristic secret classification, or warning-only fallback remains. Verify: `cargo test -p harness-agents runtime_fingerprint`, `cargo test -p harness-agents`, and `cargo check -p harness-agents --all-targets`.
- [ ] `SP1733-T3` — Owner: boundary contract worker. Dependencies: SP1733-T1 and SP1733-T2. Covers: B-002 and B-016. Done when: a `#[cfg(test)]`-only exhaustive workflow `RuntimeKind` mapping proves the three local kinds map one-to-one and `AnthropicApi`/`RemoteHost` are not local executables; production call-site audit proves there is no snapshot, server, workflow-runtime, task-runner, `CodeAgent`, `AgentAdapter`, CLI, HTTP, persistence, or migration consumer; and the implementation diff matches the twelve authorized paths exactly. Verify: `cargo test -p harness-server runtime_fingerprint_runtime_kind_contract_is_exhaustive --lib` plus the manifest and `rg` audits described below.
- [ ] `SP1733-T4` — Owner: verification and handoff owner. Dependencies: SP1733-T1 through SP1733-T3. Covers: B-001 through B-016. Done when: formatting, focused suites, both package suites, workspace check, workspace clippy, file-size limits, changed-file manifest, call-site audit, independent review, every valid PR #1859 review finding, current-head CI, Gemini review, and repository ruleset approval all pass without weakened assertions; the original PR may close GH-1733 only after all gates pass. Verify: run every command and audit in Required Verification on one current implementation head, then collect fresh PR-gate evidence.

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
- [ ] Run the repository SpecRail structural checker if it exists on the
      implementation base. If it remains removed from current `main`, perform
      an independent structural review of the complete manifest and B-001
      through B-016 coverage and do not claim that a removed checker passed.
- [ ] Re-evaluate all current review threads on PR #1859, resolve only findings
      demonstrably addressed on the current head, and post no comment or reply
      without separate authorization.
- [ ] Wait for current-head CI, independent native review, Gemini review, and
      repository ruleset approval before merge.

## Handoff Notes

- PR #1859 remains the sole implementation PR and must be repaired on its
  original branch after the readiness gate opens.
- `runner_observed` describes evidence strength and trust; it never replaces
  repository, user-global, admin, system, runtime, or genuine runner ownership.
- A bare configured command resolves only the first candidate selected by the
  actual native launch contract. It does not authorize PATH scanning,
  candidate execution, a shell, `which`, or a package manager.
- `codex.cloud.setup_secret_env` is an unconditional exclusion set. Setup
  values never enter evidence or the version child, regardless of key spelling.
- Probe `PATH` is the same sanitized PATH used for resolution. Other values
  require an explicit typed declaration and independent probe-exposure choice.
- Executable size, metadata, and digest come from one opened regular-file
  handle. Path identity is checked before and after the supervised child, and a
  mismatch discards version evidence.
- The combined stdout/stderr cap is a read-time memory bound. Timeout, output
  overflow, cancellation, or read failure terminates and reaps the process
  group before the API returns.
- MCP descriptions remain exact. Schema arrays are reordered only at the six
  approved schema-set locations; annotation, extension, and ordered-schema
  arrays retain order.
- This issue delivers producer APIs only. ASC-005 owns snapshot consumption and
  ASC-026 owns native snapshot/diff commands.
