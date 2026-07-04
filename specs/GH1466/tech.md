# Tech Spec

## Linked Issue

GH-1466

## Product Spec

See `product.md`.

## Codebase Context

| Area | Files | Current behavior | Why relevant |
| --- | --- | --- | --- |
| Workspace cleanup | `crates/harness-server/src/workspace_helpers.rs` (`cleanup_workspace_path`) | Removes worktrees, prunes stale metadata | Primary deletion path to gate |
| Workspace create/reconcile | `crates/harness-server/src/workspace_create.rs`, `workspace_startup_reconcile_tests.rs` | Reconciles registered worktrees vs disk | Lease/registration source of truth |
| Orphan reaper | `crates/harness-server/src/http/orphan_reaper.rs` | Reaps orphaned workspace resources | Second deletion path to gate |
| CLI GC | `crates/harness-cli/src/gc.rs`, `crates/harness-gc/src/gc_agent.rs` | User/agent-initiated GC | Third deletion path to gate |
| Spawn preflight | `crates/harness-server/src/task_runner/spawn.rs` (`workspace path missing before task turn`, `record_workspace_lifecycle_failure`) | Detects missing workspace before/after a turn | Existing detection layer; keep as defense-in-depth |
| Workspace lease | spawn.rs lease admission (`workspace_owner`, `run_generation`, `slot_index`) | Task admission records owner/generation | The reference data the GC check reads |
| Agent error surface | `crates/harness-agents/src/codex.rs:360-405` | Formats `failed to run codex: No such file or directory` with `current_dir_exists` diagnostics | Misattribution fix: classify missing-cwd before blaming the binary |

## Proposed Design

1. **Single reclaim gate** — one function `try_reclaim_workspace(path)`
   used by all three deletion paths. It resolves the owning task via the
   lease/registration store: non-terminal owner → skip (log task id +
   path); no resolvable owner → configurable fail-closed default (skip)
   with an explicit orphan-age override; terminal/absent owner → delete.
2. **TOCTOU guard** — the delete happens under the same store lock as the
   ownership re-check; lease admission and reclaim serialize on the store.
3. **Forced reclaim** — an explicit `force` variant terminalizes the owning
   task (`workspace_reclaimed` reason, reusing the GH-1465 terminal CAS)
   and deletes in the same operation; used by operator CLI only.
4. **Error classification** — in `codex.rs` (and the claude adapter), when
   spawn fails with ENOENT and `current_dir_exists=false`, produce
   `workspace missing: <path>` as the primary error string; keep the full
   diagnostic tail. The spawn preflight already prevents most of these; the
   classifier fixes the residual race window.
5. **GC reporting** — GC output includes skipped-live counts and paths (no
   silent caps), so shrinking reclaim volume is explainable.

## Product-to-Test Mapping

| Product invariant | Implementation area | Verification |
| --- | --- | --- |
| P1 skip live workspaces | `try_reclaim_workspace` | unit: non-terminal owner → skip + log |
| P2 terminalize-then-delete | force path | integration: forced reclaim → terminal event + dir gone, atomically |
| P3 accurate diagnostics | agent error classifier | unit: ENOENT + missing cwd → `workspace missing` |
| P4 GC visibility | GC report | test asserts skipped items appear in report |
| TOCTOU guard | store-lock re-check | concurrency test: terminalize racing reclaim |

## Data Flow

GC/reaper/CLI → `try_reclaim_workspace` → lease/task store lookup (under
lock) → skip | delete | terminalize+delete → GC report + task events.

## Alternatives Considered

- Filesystem lock files inside each workspace — rejected: stale locks after
  crashes recreate the same ambiguity; the task store already knows
  ownership.
- Only improving the error message — rejected: fixes triage, not the data
  loss; detection without prevention keeps burning agent spend.
- Refcounting workspaces across multiple harness data dirs — rejected:
  multi-instance shared roots are declared unsupported; fail-closed skip is
  the guard.

## Risks

- Fail-closed default may leave true orphans on disk — mitigated by the
  age-based override and GC report visibility.
- Coupling to GH-1465's terminal CAS — forced reclaim depends on it; land
  GH-1465 T001 first or vendor a minimal terminal-emit helper.
- April-era evidence predates current main — the spawn preflight has since
  landed; this spec targets the prevention gap that remains (deletion paths
  do not consult leases), which holds on today's tree (no reference check
  found in `gc.rs` / `orphan_reaper.rs` / `cleanup_workspace_path`).

## Test Plan

- [ ] Unit: reclaim gate decision table (non-terminal / terminal / unresolvable owner / forced).
- [ ] Concurrency: terminalize vs reclaim race under the store lock.
- [ ] Integration: GC run over mixed live/dead workspaces; forced reclaim end-to-end; error classifier on ENOENT.
