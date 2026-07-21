# Task Plan

## Linked Issue

GH-1434

## Spec Packet

- Product: `specs/GH1434/product.md`
- Tech: `specs/GH1434/tech.md`

## Implementation Tasks

- [x] `SP1434-T001` Owner: `observe` | Dependencies: none | Done when: usage probes cover thread/turn route dispatch and public entry points for `thread_manager`, `task_db`, legacy `task_executor`, `task_runner`, `harness-eval`, and `review_store`; `probe_report` can be emitted and queried | Verify: `cargo test --package harness-server probe`
- [x] `SP1434-T002` Owner: `ops` | Dependencies: none | Done when: `scripts/archive-phase1-data.sh` has produced thread/task dump artifacts and restore instructions for the deletion target | Verify: `bash scripts/archive-phase1-data.sh && test -s archives/phase1-*/RESTORE.md`
- [x] `SP1434-T003` Owner: `gate` | Dependencies: `SP1434-T001` | Done when: probes have run for at least 7 days with zero counts for the deletion surface, or a maintainer records an explicit GH-1434 waiver with scope and reason | Verify: attach `probe_report` query output or waiver URL to each removal PR
- [x] `SP1434-T004` Owner: `workspace` | Dependencies: `SP1434-T003` | Done when: `harness-eval` is removed from workspace members and related references are cleaned while eval-store data remains untouched | Verify: `cargo test --workspace && ! grep -r harness-eval crates/*/Cargo.toml Cargo.toml`
- [x] `SP1434-T005` Owner: `server` | Dependencies: `SP1434-T003` | Done when: `review_store` modules and router/handler references are removed | Verify: `cargo test --workspace && test "$(rg -l review_store crates | wc -l)" -eq 0`
- [x] `SP1434-T006` Owner: `consumers` | Dependencies: runtime replacement APIs for any live compatibility behavior being moved | Done when: CLI, dashboard, and websocket references to thread/turn/task endpoints are removed or repointed to workflow-runtime equivalents before the old endpoints are deleted | Verify: `cargo test --workspace` plus dashboard first-viewport smoke
- [x] `SP1434-T007` Owner: `server` | Dependencies: `SP1434-T003`, `SP1434-T006` | Done when: thread/turn RPC methods are removed from protocol definitions, router registration, and handlers; thread persistence is removed; the live in-memory role is retained or safely inlined | Verify: `cargo test --workspace && ! rg 'Thread(Start|Resume|Fork|List|Delete|Compact)|Turn(Start|Steer|Cancel|Status|RespondApproval)' crates/harness-protocol/src/methods.rs && ! rg 'Method::(Thread(Start|Resume|Fork|List|Delete|Compact)|Turn(Start|Steer|Cancel|Status|RespondApproval))' crates/harness-server/src/router crates/harness-server/src/handlers && ! rg 'thread_db|ThreadDb' crates --glob '!CHANGELOG.md'`
- [x] `SP1434-T008` Owner: `server` | Dependencies: `SP1434-T003`, `SP1434-T006`, extraction evidence for live workflow-runtime turn-engine dependencies | Done when: dependency scanning identifies live `task_*` references; live pieces are moved without semantic changes into workflow-runtime-adjacent modules; removable task-layer code is deleted | Verify: `cargo test --workspace` plus a PR migration table showing moved code and unchanged behavior
- [x] `SP1434-T009` Owner: `docs` | Dependencies: all removal PRs | Done when: CHANGELOG documents removed RPC methods, task endpoint compatibility changes, and archive/restore instructions; final summary reports net deleted LOC | Verify: reviewer checks method list and `git diff --shortstat`
- [x] `SP1434-T010` Owner: `tests` | Dependencies: all removal PRs | Done when: full workspace tests, clippy, and smoke checks pass | Verify: `cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings`

## Parallelization

- Lane A status: `SP1434-T001` landed in PR #1453. `SP1434-T002` completed with
  archive artifact `archives/phase1-20260716T092438Z`, and the maintainer waiver
  satisfies `SP1434-T003` for T004/T005.
- Lane B status: `SP1434-T004` landed in PR #1663, and `SP1434-T005` removes the
  independent review-store surface while leaving its Postgres schema untouched.
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

- PR #1453 is the evidence for `SP1434-T001` and the archive tooling. Archive
  execution completed on 2026-07-16 at `archives/phase1-20260716T092438Z`.
  The scratch restore rehearsal completed on 2026-07-21 against PostgreSQL
  16.14: all 29 tables and 1,689 rows matched the archived counts with an empty
  diff. The evidence and archive SHA-256 are recorded on GH-1434:
  <https://github.com/majiayu000/harness/issues/1434#issuecomment-5033352086>.
  The first attempt also proved that selected-table dumps omit schema creation;
  the archive tooling now generates a validated schema bootstrap and a
  deterministic row-count verification query. The failure-first evidence is
  retained in the preceding GH-1434 evidence comment.
- The explicit maintainer waiver on GH-1434 satisfies `SP1434-T003` for T004
  and T005 based on the recorded zero-traffic evidence and completed archive:
  <https://github.com/majiayu000/harness/issues/1434#issuecomment-4993856096>.
- Dashboard consumers moved from `/tasks` to runtime-native submission routes in
  `SP1434-T006`; PR #1706 subsequently removed the compatibility paths.
- The GH-1434 maintainer comment from 2026-07-03 narrows the deletion boundary:
  `task_executor/turn_lifecycle.rs` and `task_executor/helpers.rs` are live,
  and the in-memory `thread_manager` role is live. Treat that as boundary
  evidence, not as a broad deletion waiver.
- The GH-1434 gate comment from 2026-07-05 records that deletion PRs need either
  7 days of probe evidence or an explicit waiver with scope and reason.
- `/api/workflows/runtime/submissions` is now the only public submission list,
  detail, and stream surface. Workflow actions use `workflow_id`.
- `SP1434-T004` landed under the explicit maintainer waiver recorded on GH-1434
  (<https://github.com/majiayu000/harness/issues/1434#issuecomment-4993856096>),
  which waives the 7-day probe window for T004/T005 based on zero-count
  `harness_eval` probe evidence
  (<https://github.com/majiayu000/harness/issues/1434#issuecomment-4990260785>)
  and the archive artifact `archives/phase1-20260716T092438Z`. The removal
  deleted the `harness-eval` crate, the server eval-store module and
  `/api/evals` routes, the dashboard Evals views, and the
  `evaluate_pr_repair` script family. The `eval_store` Postgres schema and its
  data are untouched, and the `UsageProbeSurface::HarnessEval` probe variant is
  retained for continued zero-count reporting.
- `SP1434-T005` removes the local review findings store, its startup/status
  wiring, and its dependent persistence/auto-fix path. Periodic review task
  execution, synthesis, structured output parsing, and watermark behavior stay
  in place. The archived `review_store` Postgres schema and data are untouched.
- `SP1434-T006` landed in PR #1702. Dashboard submission list/detail, stream,
  proof, artifact, merge, and cancel consumers now use workflow-runtime-backed
  APIs, and the first-viewport smoke passed before the legacy lifecycle RPC
  removal began.
- `SP1434-T007` uses the scoped maintainer waiver recorded on GH-1434
  (<https://github.com/majiayu000/harness/issues/1434#issuecomment-5006185197>).
  Harness-owned lifecycle requests and thread persistence are removed. The
  in-memory `ThreadManager`, workflow-runtime turn engine and notifications,
  and Codex app-server adapter protocol remain live. Protocol rejection tests
  retain the removed method strings as negative compatibility fixtures. The
  published Python and TypeScript SDKs submit and poll workflow-runtime HTTP
  submissions, live approval responses use the authenticated runtime HTTP
  endpoint, and the unwritten context-manifest getter is removed with its sole
  legacy writer.
- `SP1434-T008` landed in PR #1706. The legacy `/tasks` compatibility routes,
  background recovery loops, task retention, and legacy remote-host task claim
  path are removed. Runtime submission request/state models now live beside the
  workflow-runtime submission layer. Review/planner execution policy and review
  queue serialization remain enforced by the workflow runtime.
- `SP1434-T009` records the exact removed RPC and HTTP compatibility surfaces,
  the archive restore safety contract, and the Phase 1 implementation total:
  34,156 deletions minus 11,246 additions equals 22,910 net deleted lines
  across PRs #1663, #1701, #1702, #1703, and #1706.
- `SP1434-T010` passed `cargo fmt --all -- --check`,
  `cargo clippy --workspace --all-targets -- -D warnings`, PostgreSQL-backed
  `cargo test --workspace`, both SpecRail checks, and runtime smoke for serve,
  status, `pr fix --help`, health, dashboard first viewport, runtime submission
  list, intake status, and a signed GitHub webhook ping.
