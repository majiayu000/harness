# Tech Spec

## Linked Issue

GH-1434

## Product Spec

`specs/GH1434/product.md`

## Current System

| Area | Files | Current behavior | Phase 1 relevance |
| --- | --- | --- | --- |
| Thread/turn RPCs | `crates/harness-protocol/src`, `crates/harness-server/src/router/mod.rs`, `crates/harness-server/src/handlers/thread.rs` | JSON-RPC lifecycle methods for thread and turn management. | Primary deletion surface after probe/waiver gate. |
| Thread state | `crates/harness-server/src/thread_manager.rs`, `crates/harness-server/src/thread_db.rs`, thread manager tests | In-memory thread state plus persistence. Maintainer evidence says the in-memory role is still hot for workflow-runtime jobs. | Delete RPC and persistence only after preserving the live in-memory role. |
| Task layer | `crates/harness-server/src/task_db.rs`, `crates/harness-server/src/task_executor/`, `crates/harness-server/src/task_runner/`, task queue tests | Legacy task state machine, plus compatibility APIs used by the dashboard and runtime submission handles. | Delete only after dependency scanning and replacement runtime APIs exist. |
| Eval | `crates/harness-eval`, eval HTTP/API handlers, eval store schema references | Evaluation crate and routes with zero cited production runs. | Independent removal surface after probe/waiver gate. |
| Review store | `crates/harness-server/src/review_store/`, review-store schema usage | Local review findings store with zero cited findings. | Independent removal surface after probe/waiver gate. |
| Consumers | `crates/harness-cli/src/main.rs`, `crates/harness-server/src/http/http_router.rs`, `crates/harness-server/src/websocket.rs`, `web/` | CLI, HTTP, websocket, and dashboard references to task/thread/turn surfaces. | Must move before endpoints are deleted. |
| Workflow runtime | `crates/harness-workflow`, `crates/harness-server/src/workflow_runtime_*`, runtime HTTP routes | Authoritative production workflow path. | Retained behavior baseline. |
| Probes | `harness_core::usage_probe`, router dispatch, public module entry points | PR #1453 added probe counters and `probe_report`. | Provides deletion evidence after the required observation window. |

## Proposed Design

Phase 1 remains a sequenced contraction, not a single large deletion PR.

1. Safety probes and archive tooling land first. PR #1453 has already added
   `UsageProbe`, `probe_report`, target-surface instrumentation, and
   `scripts/archive-phase1-data.sh`.
2. The deletion gate is evaluated before each removal PR. A removal PR must
   cite either:
   - at least 7 days of zero-count probe evidence for the surface being
     removed, or
   - an explicit GH-1434 maintainer waiver with scope and reason.
3. Archive execution precedes related code deletion. The script should dump the
   thread/task data and generate restore instructions. Tables remain in place.
4. Removal is split into independently revertible PRs:
   - remove `harness-eval`
   - remove `review_store`
   - remove thread/turn RPCs, router registrations, handlers, and persistence
   - remove legacy task-layer code after extracting or retaining live
     workflow-runtime turn-engine dependencies
5. Consumer migration precedes endpoint removal. Dashboard, CLI, websocket,
   protocol, and router references must be moved to workflow-runtime
   equivalents before the corresponding legacy endpoints disappear.
6. The final PR or summary documents API contraction, restore instructions, and
   net deleted LOC.

## Data Flow

Probe data flows from atomic counters to daily `probe_report` events. Removal
PRs reference those reports as human-readable evidence.

Archive data flows from Postgres through `pg_dump` into a local archive
directory with restore instructions. Phase 1 does not mutate production tables.

Workflow-runtime submission, scheduling, job execution, merge, cancel, and
dashboard observation remain on the workflow-runtime store and runtime HTTP
routes. Any consumer moved away from `/tasks` must preserve equivalent behavior
for submission handle lookup, status, detail, streaming logs, artifacts, proof,
merge, and cancel before the compatibility endpoint can be removed.

## Alternatives Considered

- Plugin extraction: rejected by the Phase 1 decision because git history is the
  archive and maintaining a plugin surface would preserve unnecessary cost.
- Table rename or table drop: rejected for Phase 1 because running binaries may
  still expect the existing schema.
- One large deletion PR: rejected because it is hard to review and not locally
  revertible.
- Deleting without probes: rejected because event-store evidence can miss hidden
  traffic.
- Removing dashboard `/tasks` references immediately: rejected until equivalent
  workflow-runtime APIs cover submit, list/detail, stream, proof, artifacts,
  merge, and cancel semantics.

## Risks

- Security: removal reduces exposed surface area, but any temporary replacement
  APIs must preserve existing auth rules.
- Compatibility: thread/turn RPC removal and task endpoint changes are breaking
  changes and require release notes.
- Performance: probe counters are low overhead; deletion should reduce build and
  runtime maintenance cost.
- Maintenance: `task_executor` contains both legacy code and live
  workflow-runtime turn-engine code, so dependency scanning and extraction
  evidence are mandatory.

## Test Plan

- [x] Probe tests for `UsageProbe` and `probe_report` behavior landed with
      PR #1453.
- [x] Removal PRs run focused package tests for the touched crate or module.
- [x] Removal PRs run `cargo test --workspace`.
- [x] Final readiness runs `cargo clippy --workspace --all-targets -- -D warnings`.
- [x] Smoke checks cover serve startup, status, `pr fix --help`, webhook health,
      and dashboard first viewport.
- [x] Archive creation produced a dump, restore instructions, and table counts
      before related code was deleted, as recorded in the archive evidence:
      <https://github.com/majiayu000/harness/issues/1434#issuecomment-4990260785>.
- [ ] A scratch-database restore rehearsal is not evidenced. The referenced
      operator-owned archive is not present in this PR worktree, and the scoped
      waiver does not include restore command output or row-count comparisons.

## Rollback Plan

Each removal surface is delivered in a separate PR so it can be reverted
independently. Data rollback is not needed for Phase 1 because production tables
are not dropped or renamed. If a replacement consumer path regresses, revert the
consumer migration or re-enable the compatibility route while preserving the
probe evidence.
