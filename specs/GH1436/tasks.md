# Task Plan

## Linked Issue

GH-1436

## Spec Packet

- Product: `product.md`
- Tech: `tech.md`

## Implementation Tasks

- [ ] `SP1436-T1` Owner: agent — Add legacy (unregistered) schema discovery to `reap_orphaned_path_schemas_with_workspace_roots` reusing the `pg_schema_cleanup_plan` scan, with hash-recompute subtraction of live workspaces and bounded batch drops. Done when: integration tests cover reaped/kept/ambiguous cases. Verify: `cargo test -p harness-core db_pg_schema_registry`
- [ ] `SP1436-T2` Owner: agent — Extend `PgSchemaReapReport` with `legacy_scanned`/`legacy_reaped`, thread real workspace roots + new config keys (`orphan_reaper_legacy_enabled`, `orphan_reaper_legacy_batch`) from `WORKFLOW.md` config into `orphan_reaper.rs`, and log per-class counts each tick including idle-tick debug liveness. Done when: reaper log line shows both classes and config defaults documented in `config/default.toml.example`. Verify: `cargo test -p harness-server orphan_reaper`
- [ ] `SP1436-T3` Owner: agent — Align `harness-pg-schema-cleanup` CLI and `scripts/pg-orphan-cleanup.sh` docs with the automatic legacy semantics. Done when: CLI dry-run output labels legacy candidates identically to server logs. Verify: `cargo test -p harness-cli`

## Parallelization

T1 (crates/harness-core) and T3 (crates/harness-cli, scripts/) touch disjoint files and can run in parallel; T2 depends on T1's report type change.

## Verification

- [ ] `SP1436-T4` Owner: agent — Full-tree verification and production dry-run. Done when: workspace check/test are green and the dry-run candidate count matches expectations before enabling apply. Verify: `RUSTFLAGS="-Dwarnings" cargo check --workspace --all-targets && cargo test --workspace`

## Handoff Notes

Root cause evidence: prod scan 2026-07-03 — registry rows 1,133 vs 16,978 `h<hex>` namespaces; running reaper is registry-driven only (`db_pg_schema_registry.rs:619-627`). Only one reaper log line in 8.7 h uptime — verify tick loop recurrence while in there (`orphan_reaper.rs`). Decision: direct legacy scan chosen over registration backfill (see tech.md Alternatives).
