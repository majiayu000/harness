# Tech Spec

## Linked Issue

GH-1463

## Product Spec

See `product.md`.

## Codebase Context

| Area | Files | Current behavior | Why relevant |
| --- | --- | --- | --- |
| Catch-all | `crates/harness-server/src/http/background.rs` (3,509 lines, 18 spawn sites) | dispatch tick, backlog tick, workflow-config polling, retry, reconciliation wiring, etc. | split target |
| Precedent | `crates/harness-server/src/http/orphan_reaper.rs` | one loop, own module, weak-ref AppState pattern | template for extraction |
| Spawner | `http/mod.rs` | spawns loops at startup | registry call site |

## Proposed Design

1. Create `http/background/` with one module per loop (dispatch_tick,
   backlog_tick, workflow_config_poll, retry_tick, ...), each following the
   orphan_reaper.rs shape (own module, config reload per tick where present,
   `Arc::downgrade` AppState pattern preserved).
2. `http/background/mod.rs` exposes `spawn_all(state) -> Vec<LoopHandle>` —
   the single registry; startup calls it instead of ad-hoc spawns.
3. Move loops one PR-reviewable tranche at a time (3-4 loops per commit),
   pure motion, no signature changes beyond visibility.
4. Remove the U-16 exemption for background.rs from CLAUDE.md when done.

## Product-to-Test Mapping

| Product invariant | Implementation area | Verification |
| --- | --- | --- |
| P1 pure motion | per-loop modules | existing tests pass unchanged; `RUSTFLAGS="-Dwarnings" cargo check --workspace --all-targets` |
| P2 single registry | background/mod.rs | unit test: registry lists all loops |
| P3 size ceiling | file layout | line-count check in review |

## Alternatives Considered

- Full harness-runtime crate split — larger payoff but multi-week churn and
  merge-conflict exposure; per-loop file split first recovers readability
  cheaply and does not preclude the crate split later.
