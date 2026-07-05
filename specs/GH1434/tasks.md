# Task Plan

## Linked Issue

GH-1434

## Spec Packet

- Product: `specs/GH1434/product.md`
- Tech: `specs/GH1434/tech.md`

## Implementation Tasks

- [x] `SP1434-T001` Owner: `observe` | Dependencies: none | Done when: usage probes cover thread/turn route dispatch and public entry points for `thread_manager`, `task_db`, legacy `task_executor`, `task_runner`, `harness-eval`, and `review_store`; `probe_report` can be emitted and queried | Verify: `cargo test --package harness-server probe`
- [ ] `SP1434-T002` Owner: `ops` | Dependencies: none | Done when: `scripts/archive-phase1-data.sh` has produced thread/task dump artifacts and restore instructions for the deletion target | Verify: `bash scripts/archive-phase1-data.sh && test -s archives/phase1-*/RESTORE.md`
- [ ] `SP1434-T003` Owner: `gate` | Dependencies: `SP1434-T001` | Done when: probes have run for at least 7 days with zero counts for the deletion surface, or a maintainer records an explicit GH-1434 waiver with scope and reason | Verify: attach `probe_report` query output or waiver URL to each removal PR
- [ ] `SP1434-T004` Owner: `workspace` | Dependencies: `SP1434-T003` | Done when: `harness-eval` is removed from workspace members and related references are cleaned while eval-store data remains untouched | Verify: `cargo test --workspace && ! grep -r harness-eval crates/*/Cargo.toml Cargo.toml`
- [ ] `SP1434-T005` Owner: `server` | Dependencies: `SP1434-T003` | Done when: `review_store` modules and router/handler references are removed | Verify: `cargo test --workspace && test "$(rg -l review_store crates | wc -l)" -eq 0`
- [ ] `SP1434-T006` Owner: `consumers` | Dependencies: runtime replacement APIs for any live compatibility behavior being moved | Done when: CLI, dashboard, and websocket references to thread/turn/task endpoints are removed or repointed to workflow-runtime equivalents before the old endpoints are deleted | Verify: `cargo test --workspace` plus dashboard first-viewport smoke
- [ ] `SP1434-T007` Owner: `server` | Dependencies: `SP1434-T003`, `SP1434-T006` | Done when: thread/turn RPC methods are removed from protocol definitions, router registration, and handlers; thread persistence is removed; the live in-memory role is retained or safely inlined | Verify: `cargo test --workspace && test "$(rg -l 'thread/start|turn/start|thread_db' crates | wc -l)" -eq 0`
- [ ] `SP1434-T008` Owner: `server` | Dependencies: `SP1434-T003`, `SP1434-T006`, extraction evidence for live workflow-runtime turn-engine dependencies | Done when: dependency scanning identifies live `task_*` references; live pieces are moved without semantic changes into workflow-runtime-adjacent modules; removable task-layer code is deleted | Verify: `cargo test --workspace` plus a PR migration table showing moved code and unchanged behavior
- [ ] `SP1434-T009` Owner: `docs` | Dependencies: all removal PRs | Done when: CHANGELOG documents removed RPC methods, task endpoint compatibility changes, and archive/restore instructions; final summary reports net deleted LOC | Verify: reviewer checks method list and `git diff --shortstat`
- [ ] `SP1434-T010` Owner: `tests` | Dependencies: all removal PRs | Done when: full workspace tests, clippy, and smoke checks pass | Verify: `cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings`

## Parallelization

- Lane A status: `SP1434-T001` landed in PR #1453, and the archive script
  prerequisite for `SP1434-T002` landed in PR #1453. Archive execution remains
  open before related code removal.
- Lane B waits for `SP1434-T003`: `SP1434-T004` and `SP1434-T005` are
  independent deletion PRs after evidence or waiver.
- Lane C is serial: `SP1434-T006` consumer migration precedes `SP1434-T007`
  thread/turn deletion and `SP1434-T008` task-layer deletion.
- `SP1434-T009` and `SP1434-T010` are final closeout tasks after deletion PRs.

## Verification

- `python3 checks/check_workflow.py --repo .`
- `python3 checks/check_workflow.py --repo . --spec-dir specs/GH1434`
- For spec-only updates, `cargo fmt --all -- --check` is sufficient before PR
  creation. Code removal PRs must run the focused tests listed above plus the
  final workspace gates.

## Handoff Notes

- PR #1453 merged the probe/archive-script prerequisite but does not satisfy
  the 7-day deletion gate by itself.
- PR #1453 is the evidence for `SP1434-T001` and for the archive-script
  prerequisite of `SP1434-T002`. It is not evidence that archive execution has
  completed.
- `SP1434-T003` is blocked as of 2026-07-05 because PR #1453 has not run for 7
  days and no broad Phase 1 deletion waiver is recorded.
- Dashboard `/tasks` references are live compatibility paths for
  workflow-runtime submission handles, details, streams, proof, artifacts,
  merge, and cancel. Do not remove them until equivalent runtime APIs exist and
  tests cover the replacement.
- The GH-1434 maintainer comment from 2026-07-03 narrows the deletion boundary:
  `task_executor/turn_lifecycle.rs` and `task_executor/helpers.rs` are live,
  and the in-memory `thread_manager` role is live. Treat that as boundary
  evidence, not as a broad deletion waiver.
- The GH-1434 gate comment from 2026-07-05 records that deletion PRs need either
  7 days of probe evidence or an explicit waiver with scope and reason.
- `/tasks` is currently a workflow-runtime compatibility API for dashboard
  submission handles and observation. Removing it requires replacement runtime
  APIs first.
