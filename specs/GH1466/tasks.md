# Task Plan

## Linked Issue

GH-1466

## Spec Packet

- Product: `product.md`
- Tech: `tech.md`

## Implementation Tasks

- [ ] `SP1466-T001` Owner: server-workspace | Done when: `try_reclaim_workspace` exists with the decision table (non-terminal owner → skip+log, unresolvable owner → fail-closed skip with age override, terminal/absent → delete) and delete re-checks ownership under the store lock | Verify: `cargo test -p harness-server reclaim_gate`
- [ ] `SP1466-T002` Owner: server-workspace | Done when: all three deletion paths (`cleanup_workspace_path`, orphan reaper, CLI/agent GC) route through the reclaim gate | Verify: `cargo test -p harness-server reclaim_gate_wiring` + grep shows no direct `remove_dir_all` on workspace roots outside the gate
- [ ] `SP1466-T003` Owner: server-workspace | Done when: forced reclaim terminalizes the owning task with reason `workspace_reclaimed` and deletes atomically (depends on GH-1465 terminal CAS or a minimal helper) | Verify: `cargo test -p harness-server forced_reclaim`
- [ ] `SP1466-T004` Owner: agents | Done when: ENOENT spawn failures with `current_dir_exists=false` surface `workspace missing: <path>` as the primary error in both codex and claude adapters, keeping the diagnostic tail | Verify: `cargo test -p harness-agents missing_cwd_classifier`
- [ ] `SP1466-T005` Owner: server-workspace | Done when: GC output reports skipped-live workspaces (count + paths + owning tasks) | Verify: `cargo test -p harness-server gc_report_skips`
- [ ] `SP1466-T006` Owner: tests | Done when: a concurrency test covers terminalize-racing-reclaim (TOCTOU) and an integration test runs GC over mixed live/dead workspaces | Verify: `cargo test -p harness-server reclaim_race gc_mixed`

## Parallelization

- T001 first; T004 ∥ T001 (disjoint crates: harness-agents vs harness-server).
- T002 ∥ T005 after T001; T003 after T001 (and GH-1465 T001 if landed).
- T006 last.

## Verification

- `cargo test --workspace`
- Manual: run `harness gc` against a data dir with one in-flight task — its workspace survives and appears in the skip report.

## Handoff Notes

- Fail-closed is the default for unresolvable owners; do not "optimize" it to fail-open — the April incident class (28/34 failures) came from exactly that.
- Multi-instance shared workspace roots remain unsupported; the gate must not be weakened to accommodate them.
