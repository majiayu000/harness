# Product Spec

## Linked Issue

GH-1434

## User Problem

Kernel-contraction Phase 1 removes obsolete orchestration layers after the
workflow runtime became the factual production path. The current repository
still carries multiple older state machines:

- workflow runtime: 216,161 runtime events, 67,436 jobs, and 1,877 workflows in
  the production evidence cited by GH-1434
- thread/turn lifecycle RPCs, `thread_manager`, and `thread_db`: 9 historical
  threads
- task layer (`task_db`, legacy `task_executor` paths, and `task_runner`): 62
  historical tasks
- `harness-eval`: 0 eval runs
- `review_store`: 0 review findings while review evidence is handled by
  GitHub bots

Keeping these layers makes the public surface larger, slows maintenance, and
makes it unclear which path new contributors should use. The approved Phase 1
direction is direct deletion after safety evidence, not plugin extraction.

## Goals

1. Remove zero-traffic surfaces: thread/turn RPCs, deletable task-layer paths,
   `harness-eval`, and `review_store`.
2. Land safety probes before deletion and require at least 7 days of zero-count
   probe evidence, or a maintainer waiver recorded in GH-1434 with scope and
   reason, before each removal PR lands.
3. Archive thread/task data before deleting code. The phase may dump data for
   recovery, but it must not drop or rename Postgres tables.
4. Keep workflow runtime behavior unchanged for serve, status, PR-fix flows,
   webhook intake, and dashboard workflow-runtime views.

## Non-Goals

- Phase 2 verify-over-envelope work.
- Phase 3 intelligence-layer removal for non-execpolicy rules, skills, GC,
  learn, self-evolution, `q_value`, or `complexity_router`.
- Dropping, renaming, or migrating production Postgres tables in this phase.
- Changing workflow-runtime dispatch semantics.
- Changing the GH-1430 circuit-breaker behavior.
- Restyling retained modules.

## User-Visible Behavior

1. Removal PRs are blocked until they cite either zero-count probe evidence from
   at least 7 days of probe runtime or an explicit maintainer waiver recorded on
   GH-1434.
2. Thread/task archive data must exist before deletion PRs that remove the
   corresponding code. The archive instructions must include restore commands.
3. Each removal surface remains independently revertible. At minimum,
   `harness-eval`, `review_store`, thread/turn RPC removal, and task-layer
   removal are separate PRs.
4. Retained workflow-runtime paths behave the same before and after deletion:
   `harness serve`, `harness status`, `harness pr fix --help`, webhook intake,
   and the dashboard first viewport must still work.
5. Consumers must move before endpoints disappear. Dashboard, CLI, websocket,
   protocol, and router references to removed surfaces must either be removed or
   repointed to workflow-runtime equivalents before the old endpoints are
   deleted.
6. Public API contraction is documented. The breaking-change note lists the
   removed thread/turn RPC methods and any task endpoint compatibility changes.

## Acceptance Criteria

- [x] Usage probes have landed for the Phase 1 target surfaces. PR #1453 added
      `UsageProbe`, `probe_report`, and target-surface instrumentation.
- [ ] Deletion PRs cite at least 7 days of zero-count `probe_report` evidence
      or a maintainer waiver recorded on GH-1434 with scope and reason.
- [ ] Thread/task archive execution has produced dump artifacts and restore
      instructions before related code is removed.
- [ ] After all removal PRs merge, `harness-eval` is no longer a workspace
      member, `review_store` is gone, removable thread/turn and task layers are
      gone, and no dangling protocol/router/CLI/dashboard references remain.
- [ ] The final Phase 1 summary reports net deleted LOC. The expected target is
      more than 10,000 net deleted LOC.
- [ ] `cargo test --workspace` exits 0.
- [ ] Smoke checks pass: serve startup, status, `pr fix --help`, webhook
      health, and dashboard first viewport.

## Edge Cases

- Hidden traffic can bypass older event-store evidence. Probe coverage is
  required at RPC entry points and public module entry points before deletion.
- `task_executor` is not wholesale deletable. Maintainer evidence on GH-1434
  says `task_executor/turn_lifecycle.rs` and `task_executor/helpers.rs` are live
  workflow-runtime turn-engine code and must be extracted or retained.
- `thread_manager` has a hot in-memory role for workflow-runtime jobs. Phase 1
  may delete thread/turn RPCs and `thread_db` persistence, but the in-memory
  role must remain or be inlined safely.
- The dashboard currently uses `/tasks` as a compatibility surface for
  workflow-runtime submission handles, details, logs, proof, and artifacts.
  Deleting those consumers before equivalent runtime APIs exist would regress
  the dashboard.
- CLI compatibility changes for thread/turn/task endpoints are breaking
  changes and must be documented.

## Rollout Notes

PR #1453 provided the probe and archive-script prerequisite, but the 7-day
deletion gate is not satisfied as of 2026-07-05. Future removal PRs must either
attach fresh zero-count probe evidence from the required window or cite an
explicit maintainer waiver comment on GH-1434.
