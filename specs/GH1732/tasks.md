# Task Plan

## Linked Issue

GH-1732

## Spec Packet

- Product: `specs/GH1732/product.md`
- Tech: `specs/GH1732/tech.md`

## Implementation Tasks

- [ ] `SP1732-T1` — Owner: context-provenance implementation worker. Dependencies: approved product and tech specs. Covers: B-011, B-012, B-014, B-015. Done when: arbitrary accepted profile names use deterministic `runtime_profile/name_sha256_<digest>` locators while exact names remain in settings, the model-facing clone uses v1 while durable evidence remains v2, obsolete negative fixtures are replaced by both named acceptance tests, corrupted-provenance failure coverage remains, and `context_provenance_tests.rs` is below 800 lines. Verify: `cargo test -p harness-server context_provenance_tests::arbitrary_profile_names_use_stable_hashed_locators_and_preserve_profile_name --lib` and `cargo test -p harness-server context_provenance_tests::model_facing_prompt_uses_v1_schema_while_durable_packet_remains_v2 --lib`.
- [ ] `SP1732-T2` — Owner: runtime-profile implementation worker. Dependencies: approved product and tech specs. Covers: B-004, B-016. Done when: omitted Codex approval stays `unobserved_agent_default`, omitted Claude Code and Anthropic API approval becomes `not_applicable`, and explicit non-Codex approval remains rejected. Verify: `cargo test -p harness-server runtime_profile::tests::non_codex_omitted_approval_policy_is_not_applicable --lib` and `cargo test -p harness-server runtime_profile::tests::runtime_profile_approval_policy_rejects_non_codex_runtimes --lib`.
- [ ] `SP1732-T3` — Owner: verification and handoff owner. Dependencies: SP1732-T1 and SP1732-T2. Covers: B-001, B-002, B-003, B-004, B-005, B-006, B-007, B-008, B-009, B-010, B-011, B-012, B-013, B-014, B-015, B-016. Done when: formatting, package check, focused tests, package tests, workspace clippy, workflow validation, file-size gate, and authorized-path audit all pass on the implementation head with no weakened assertions. Verify: `cargo fmt --all -- --check`, `cargo check -p harness-server --all-targets`, `cargo test -p harness-server context_provenance --lib`, `cargo test -p harness-server runtime_profile --lib`, `cargo test -p harness-server --lib`, `cargo clippy --workspace --all-targets -- -D warnings`, and `python3 checks/check_workflow.py --repo . --spec-dir specs/GH1732`.

## Parallelization

- SP1732-T1 and SP1732-T2 may run in parallel only with the disjoint ownership
  below. Neither worker may edit the other worker's files.
- SP1732-T3 runs after both implementation tasks are committed on one
  integration head.

| Task | Writable files |
| --- | --- |
| SP1732-T1 | `crates/harness-server/src/workflow_runtime_worker/prompt_packet/context_provenance.rs`; `crates/harness-server/src/workflow_runtime_worker/prompt_packet/context_provenance_tests.rs` |
| SP1732-T2 | `crates/harness-server/src/workflow_runtime_worker/runtime_profile.rs` |
| SP1732-T3 | Read-only verification; no writable files |

No other implementation path is authorized. In particular, do not edit Cargo
files, `prompt_packet.rs`, `executor.rs`, database or protocol files, workflow
configuration, or high-context files.

## Verification

- [ ] Run each named acceptance test independently so one Cargo filter is used
      per command.
- [ ] Run `cargo fmt --all` before `cargo fmt --all -- --check`.
- [ ] Run `cargo check -p harness-server --all-targets`.
- [ ] Run `cargo test -p harness-server context_provenance --lib`.
- [ ] Run `cargo test -p harness-server runtime_profile --lib`.
- [ ] Run `cargo test -p harness-server --lib`.
- [ ] Run `cargo clippy --workspace --all-targets -- -D warnings` before push.
- [ ] Confirm
      `crates/harness-server/src/workflow_runtime_worker/prompt_packet/context_provenance_tests.rs`
      has fewer than 800 lines.
- [ ] Confirm the implementation diff contains exactly:
      `context_provenance.rs`, `context_provenance_tests.rs`, and
      `runtime_profile.rs` at the paths listed above.
- [ ] Run
      `python3 checks/check_workflow.py --repo . --spec-dir specs/GH1732`.

## Handoff Notes

- The issue is reopened and currently labeled `ready_to_spec`; spec approval
  and transition to `ready_to_implement` remain human gates.
- PR #1813 is already merged. The implementation must be a follow-up branch
  from current `origin/main`, not a rewrite or force-push of the merged branch.
- The locator hashes the exact UTF-8 profile-name bytes. It must not trim,
  normalize, case-fold, slugify, percent-encode, or expose the raw name.
- The durable packet, activity artifact, validation, and digest remain v2.
  Only the model-facing clone is reset to v1 after audit fields are removed.
- `NotApplicable` is an audit claim distinct from
  `UnobservedAgentDefault`; both return `None` from `explicit_value()`.
- The implementation PR may close GH-1732 only after every task and named
  acceptance test is complete and the repository PR gates pass.
