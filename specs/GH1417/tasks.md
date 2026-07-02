# GH1417 Tasks

Issue: `#1417`
Product spec: `specs/GH1417/product.md`
Tech spec: `specs/GH1417/tech.md`

## Status

- [ ] `SP1417-T001` Owner: `storage` | Done when: a forward-only migration adds the `(state, updated_at)` index needed by the aged-instance query, on both Postgres and SQLite paths | Verify: `cargo test --package harness-workflow`
- [ ] `SP1417-T002` Owner: `storage` | Done when: `list_aged_wait_instances(states, older_than, limit)` returns only non-terminal wait-state instances older than the threshold, with boundary tests | Verify: `cargo test --package harness-workflow`
- [ ] `SP1417-T003` Owner: `server-sweeper` | Done when: a watchdog sweeper module is spawned from server startup, logs aged `blocked`/`awaiting_feedback` instances at error level with id, definition, state, age, and bound repo/issue, and is config-gated off by default | Verify: `cargo test --package harness-server`
- [ ] `SP1417-T004` Owner: `operator-api` | Done when: the operator-monitor payload includes a `stuck_workflows` section fed by the watchdog snapshot and the web `operator_snapshot` types render it in the cockpit | Verify: `cargo test --package harness-server && (cd web && bun run test)`
- [ ] `SP1417-T005` Owner: `storage` | Done when: `prune_terminal_runtime_history(terminal_before, batch_limit)` deletes events/jobs only for fully-terminal parent/child families in bounded batches, with an active-child negative test | Verify: `cargo test --package harness-workflow`
- [ ] `SP1417-T006` Owner: `server-sweeper` | Done when: a retention sweeper module is spawned from server startup, config-gated off by default, logs per-tick deletion counts, and startup validation warns when the retention window undercuts consumer lookbacks | Verify: `cargo test --package harness-server`
- [ ] `SP1417-T007` Owner: `docs` | Done when: config reference documents both sweepers, their defaults, and the recommended enable order (watchdog first, retention second) | Verify: `test -s docs/usage-guide.md`

## Parallelization

- Lane A (T002, T003, T004): watchdog query, sweeper, operator surface.
- Lane B (T005, T006): retention mutation and sweeper.
- T001 plus config scaffolding land first as a small standalone change; after that the lanes are disjoint on files (`store_migrations.rs`, `http/mod.rs`, and config are the only shared files).

## Verification

- `cargo test --package harness-workflow --package harness-server`
- `RUSTFLAGS="-Dwarnings" cargo check --workspace --all-targets`
- Manual: seeded aged instance appears in the operator monitor; before/after row counts for a retention sweep captured in the implementation PR body.

## Handoff Notes

- Never mutate instance state from a sweeper; the only mutation channel is a decision routed through the reducer (stale-recovery precedent, commit `c6c4c15a`).
- Pruning must respect parent/child families; deleting a terminal child's history while its parent is active breaks completion-propagation evidence.
- Put each sweeper in its own module; `http/background.rs` is already at ~3.5k lines and must not grow.
- The decision-routed recovery hook is phase 2 and belongs in a separate implementation PR.
