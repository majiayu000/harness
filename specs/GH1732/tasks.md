# Task Plan

## Linked Issue

GH-1732

## Spec Packet

- Product: `specs/GH1732/product.md`
- Tech: `specs/GH1732/tech.md`

## Implementation Tasks

- [ ] `SP1732-T1` — Owner: provenance compatibility worker. Dependencies: approved product and tech specs. Covers: B-003, B-011, B-012, B-014, B-015. Done when: valid historical `runtime_profile/<exact-name>` identities are preserved; invalid names use only the disjoint `runtime_profile_name_sha256/<exact-byte-digest>` namespace with literal vectors for empty, whitespace, case, slash, UUID, and NFC/NFD inputs; the fixed-input pre-v2 golden includes a non-empty prompt template and matches the complete current model prompt; durable evidence remains v2; and the real worker failure test records no `RuntimePromptPrepared` event and invokes no agent. Verify: run each named test independently: `cargo test -p harness-server context_provenance_tests::profile_locator_preserves_valid_identity_and_hashes_invalid_exact_bytes --lib`, `cargo test -p harness-server prompt_packet::tests::model_facing_prompt_matches_frozen_v1_fixture_while_durable_packet_remains_v2 --lib`, `cargo test -p harness-server context_provenance_tests::invalid_required_provenance_aborts_packet_construction --lib`, and isolated-DB `cargo test -p harness-server provenance_failure_prevents_prompt_event_and_agent_start --lib`.
- [ ] `SP1732-T2` — Owner: runtime-capability validation worker. Dependencies: approved product and tech specs. Covers: B-004, B-016. Done when: omitted policy is `unobserved_agent_default` for Codex Exec and Codex JSON-RPC, `not_applicable` for Claude Code and Anthropic API, and locally rejected for Remote Host; direct resolution and both dispatch-policy paths reject explicit policy for every non-Codex kind; and a Codex-to-non-Codex switch without explicit policy does not inherit the Codex value. Verify: `cargo test -p harness-server runtime_profile::tests::approval_policy_resolution_matches_runtime_capability_matrix --lib`, `cargo test -p harness-server runtime_profile::tests::runtime_profile_approval_policy_rejects_non_codex_runtimes --lib`, `cargo test -p harness-server runtime_profiles::tests::runtime_dispatch_rejects_explicit_non_codex_approval_policy --lib`, and `cargo test -p harness-server runtime_profiles::tests::runtime_dispatch_kind_switch_does_not_inherit_codex_approval_policy --lib`.
- [ ] `SP1732-T3` — Owner: verification and handoff owner. Dependencies: SP1732-T1 and SP1732-T2. Covers: B-001 through B-016. Done when: existing Claude launch parity, memory-degradation, deterministic rebuild, durable event/artifact, and all new remediation tests pass without weakened assertions; formatting, package check/tests, isolated-DB server and workspace suites, workspace clippy, workflow validation, two file-size gates, and the seven-path audit pass on the implementation head. Without an isolated database, the repository DB-less pre-push suite may be recorded only as partial evidence and PostgreSQL suites remain explicitly deferred to current-head CI or an isolated database run. Verify: `cargo fmt --all -- --check`; `cargo check -p harness-server --all-targets`; `cargo test -p harness-server --lib`; `HARNESS_DATABASE_URL=<isolated-test-url> scripts/test-server-db.sh`; `HARNESS_DATABASE_URL=<isolated-test-url> cargo test --workspace -- --test-threads=1`; `cargo clippy --workspace --all-targets -- -D warnings`; and `python3 checks/check_workflow.py --repo . --spec-dir specs/GH1732`.

## Parallelization

- SP1732-T1 and SP1732-T2 may run in parallel only with the disjoint ownership
  below. Neither worker may edit the other worker's files.
- SP1732-T3 runs after both implementation tasks are committed on one
  integration head.

| Task | Writable files |
| --- | --- |
| SP1732-T1 | `crates/harness-server/src/workflow_runtime_worker/prompt_packet/context_provenance.rs`; `crates/harness-server/src/workflow_runtime_worker/prompt_packet/context_provenance_tests.rs`; `crates/harness-server/src/workflow_runtime_worker/prompt_packet_tests.rs`; `crates/harness-server/src/workflow_runtime_worker/prompt_packet/fixtures/model_facing_prompt_v1.txt`; `crates/harness-server/src/http/tests/runtime_worker_tests.rs` |
| SP1732-T2 | `crates/harness-server/src/workflow_runtime_worker/runtime_profile.rs`; `crates/harness-server/src/http/background/runtime_profiles.rs` |
| SP1732-T3 | Read-only verification; no writable files |

No other implementation path is authorized. In particular, do not edit Cargo
files, `prompt_packet.rs`, `executor.rs`, database or protocol files, workflow
configuration, or high-context files. T1 and T2 may run in parallel only with
the exact disjoint ownership above; shared verification stays with T3.

## Verification

- [ ] Run each named acceptance test independently so one Cargo filter is used
      per command.
- [ ] Run `cargo fmt --all` before `cargo fmt --all -- --check`.
- [ ] Run `cargo check -p harness-server --all-targets`.
- [ ] Run `cargo test -p harness-server context_provenance --lib`.
- [ ] Run `cargo test -p harness-server prompt_packet --lib`.
- [ ] Run `cargo test -p harness-server runtime_profile --lib`.
- [ ] Run `cargo test -p harness-server runtime_profiles --lib`.
- [ ] Run `cargo test -p harness-server repo_memory_prompt --lib`.
- [ ] Run `cargo test -p harness-server --lib`.
- [ ] With an isolated disposable PostgreSQL database, run
      `HARNESS_DATABASE_URL=<isolated-test-url> scripts/test-server-db.sh`.
- [ ] With the same isolated database, run
      `HARNESS_DATABASE_URL=<isolated-test-url> cargo test --workspace -- --test-threads=1`.
      If no isolated database is available, run the repository DB-less pre-push
      suite, record the PostgreSQL suites as deferred, and do not treat skipped
      worker tests as B-012/B-013 evidence.
- [ ] Run `cargo clippy --workspace --all-targets -- -D warnings` before push.
- [ ] Re-run
      `context_provenance_tests::claude_phase_defaults_and_explicit_overrides_match_agent_launch_provenance`,
      `context_provenance_tests::provenance_and_agent_launch_share_resolved_runtime_settings_and_reject_zero_timeout`,
      and
      `repo_memory_prompt::tests::memory_flag_degraded_enabled_store_unavailable_records_degradation`.
- [ ] Confirm `context_provenance_tests.rs` and `prompt_packet_tests.rs` each
      have fewer than 800 lines.
- [ ] Confirm the implementation diff contains exactly:
      `context_provenance.rs`, `context_provenance_tests.rs`,
      `model_facing_prompt_v1.txt`, `prompt_packet_tests.rs`,
      `runtime_profile.rs`, `runtime_profiles.rs`, and
      `runtime_worker_tests.rs` at the paths listed above.
- [ ] Run
      `python3 checks/check_workflow.py --repo . --spec-dir specs/GH1732`.

## Handoff Notes

- The issue is reopened and currently labeled `ready_to_spec`; spec approval
  and transition to `ready_to_implement` remain human gates.
- PR #1813 is already merged. The implementation must be a follow-up branch
  from current `origin/main`, not a rewrite or force-push of the merged branch.
- Preserve every historical `runtime_profile/<exact-name>` locator that already
  validates, including `codex-default` and `team/codex`. Only validation
  failures use `runtime_profile_name_sha256/<digest>`.
- The fallback hashes exact UTF-8 profile-name bytes. It must not trim,
  normalize, case-fold, slugify, percent-encode, or derive expected test digests
  with the helper under test.
- The durable packet, activity artifact, validation, and digest remain v2.
  Only the model-facing clone is reset to v1 after audit fields and
  `workflow_file.prompt_template` are removed; the non-empty template is then
  appended once. The oracle is the complete prompt fixture from `f55eea8b`,
  never a clone of the current v2 packet.
- `NotApplicable` is an audit claim distinct from
  `UnobservedAgentDefault`; both return `None` from `explicit_value()`.
- Explicit non-Codex approval policy is invalid input and must error in both
  dispatch configuration and direct resolution. An omitted policy during a kind
  switch must not inherit a Codex-only value.
- The executor-ordering test uses a job-scoped `cfg(test)` failure marker and an
  isolated database; do not replace it with global mutable state or a skipped
  no-database test.
- The implementation PR may close GH-1732 only after every task and named
  acceptance test is complete and the repository PR gates pass.
