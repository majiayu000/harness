# Task Plan

## Linked Issue

GH-1447

## Spec Packet

- Product: `product.md`
- Tech: `tech.md`

## Implementation Tasks

- [ ] `SP1447-T001` Owner: workflow | Done when: benchmark manifest schema + parser exist under an `evals/benchmarks/` convention with parse/reject unit tests | Verify: `cargo test -p harness-workflow eval_manifest`
- [ ] `SP1447-T002` Owner: workflow | Done when: scoring pure functions are ported from `harness-eval/src/scoring.rs` into the runtime eval module with a determinism test (same evidence → byte-identical result), landed before the GH-1434 crate deletion PR | Verify: `cargo test -p harness-workflow eval_scoring`
- [ ] `SP1447-T003` Owner: workflow | Done when: eval driver dispatches manifest cases through the standard runtime path with `eval_run_id` metadata, `harness-eval/` branch prefix, and draft-PR submission mode | Verify: `cargo test -p harness-workflow eval_run`
- [ ] `SP1447-T004` Owner: workflow | Done when: evidence collector maps submission/quality_gate/usage records to per-case evidence, with missing evidence yielding case failure (never a silent pass) | Verify: `cargo test -p harness-workflow eval_evidence`
- [ ] `SP1447-T005` Owner: server+cli | Done when: `harness eval run` and `harness eval diff` entries emit pass@1 / pass^k / cost / tokens and version-to-version transitions | Verify: `cargo test -p harness-cli eval_report`
- [ ] `SP1447-T006` Owner: workflow | Done when: cancel-mid-run leaves zero orphan workflows, workspaces, PRs, or schemas | Verify: `cargo test -p harness-workflow eval_cleanup`
- [ ] `SP1447-T007` Owner: docs+data | Done when: 10+ merged historical harness issues are curated into the initial manifest with pinned base commits | Verify: `harness eval run --manifest evals/benchmarks/harness-core.toml --dry-run` lists all cases

## Parallelization

- T001 and T002 are independent lanes (manifest module vs scorer module, disjoint files).
- T003/T004 depend on T001; T005 depends on T002+T004; T006 follows T003.
- T007 is data-only and parallel to every code lane.

## Verification

- `cargo test --workspace`
- Determinism: rescore fixture evidence twice, byte-identical output.
- Manual: full run of the curated manifest on the live harness, then `harness eval diff` across two runs.

## Handoff Notes

- Ordering constraint with GH-1434: port the scorer (T002) before the crate deletion lands; cross-link both PRs.
- Do not create new Postgres schemas; stay inside the workflow namespace (GH-1436 orphan-schema history is the cautionary tale).
