# Product Spec

## Linked Issue

GH-1466

## User Problem

Workspace GC/cleanup can delete directories still referenced by non-terminal
tasks. In the 2026-04-28..29 records (`harness2` data dir), 28 of 34 task
failures (82%) carried `current_dir_exists=false`: the agent phase started in
a workspace that no longer existed. The surfaced error — `failed to run
codex: No such file or directory` — misdirected triage toward the codex
binary. Current main detects the condition before a turn
(`WorkspaceLifecycle: workspace path missing before task turn`), but
detection is post-hoc: the task's in-flight work is already lost.

## Goals

- Prevent deletion of any workspace referenced by a non-terminal task
  (lease/reference check at GC time), instead of only detecting the loss
  afterwards.
- When deletion is forced (operator override, corrupt workspace), atomically
  terminalize the owning task with reason `workspace_reclaimed`.
- Keep the existing detection path as defense-in-depth with accurate
  diagnostics.

## Non-Goals

- Redesigning workspace layout, slots, or the lease data model.
- Cross-process file locking between unrelated harness installations
  (multiple data dirs sharing one workspace root is documented as
  unsupported, not engineered around).
- Orphaned Postgres schema reaping (GH-1436).

## Behavior Invariants

1. GC and every cleanup path (`cleanup_workspace_path`, orphan reaper, CLI
   `gc`) consult the task/lease store and skip workspaces owned by
   non-terminal tasks, logging the skip with the owning task id.
2. Forced reclaim transitions the owning task to a terminal state with
   reason `workspace_reclaimed` in the same operation as the deletion —
   never delete first and let the task discover it later.
3. A task failure caused by a missing workspace is reported as
   `workspace missing` (with the path), never as an agent-binary error.
4. GC reports what it skipped and why (no silent caps): operators can see
   live-workspace pressure instead of wondering why GC freed nothing.

## Acceptance Criteria

- [ ] GC run against a store with a non-terminal task leaves that task's
      workspace intact and logs the skip.
- [ ] Forced reclaim produces a terminal task event `workspace_reclaimed`
      and the deletion in one atomic step.
- [ ] A missing-workspace failure surfaces `workspace missing: <path>` in
      the task error, not a codex/claude binary error.
- [ ] Existing spawn preflight detection stays green as the second layer.

## Edge Cases

- Task terminalizes between the lease check and the delete — delete path
  re-checks under the lease/store lock (TOCTOU guard).
- Stale leases from crashed runs — leases carry owner-session/generation;
  reclaim allowed when the owning session is provably dead, still with the
  terminalize-first rule.
- Workspace root shared by a second harness instance — out of scope for
  locking, but the lease check must fail closed (skip deletion) when the
  owner cannot be resolved.

## Rollout Notes

Behavioral tightening of GC; may reduce space reclaimed per run (skipped
live workspaces are now visible in GC output). Document the
`workspace_reclaimed` reason string in CHANGELOG.
