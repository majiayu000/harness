# Tech Spec

## Linked Issue

GH-1646

## Product Spec

`specs/GH1646/product.md`

## Codebase Context

Verified at `origin/main` commit `203cd2ce`.

| Area | Anchor | Current behavior | Target owner |
| --- | --- | --- | --- |
| Store facade and types | `crates/harness-server/src/task_runner/store.rs:1-131` | Defines `TaskStore`, summary filters/cursor, constants, recovered-PR records, and shared imports. | Root facade keeps stable types, fields, constants, module wiring, and narrow re-exports. |
| Construction and startup | `store.rs:132-291` | Opens `TaskDb`, replays events, hydrates cache, and runs startup recovery. | `store/startup.rs` |
| Reads and duplicate queries | `store.rs:292-660` | Resolves cache/DB reads, terminal rows, duplicates, summaries, paging, and external statuses. | `store/queries.rs` |
| Runtime-host ownership | `store.rs:661-813` | Counts, claims, releases, and bulk-releases runtime-host leases. | `store/runtime_hosts.rs` |
| Projections and metrics | `store.rs:814-1047` | Produces PR URL, sibling, dashboard, completion-rate, and LLM usage projections. | `store/projections.rs` |
| Runtime control | `store.rs:1048-1164` | Lists child tasks and owns stream senders, abort handles, and rate limiting. | `store/runtime_control.rs` |
| Persistence and checkpoints | `store.rs:1165-1432` | Stores artifacts/prompts, event-log entries, tasks, checkpoints, external IDs, and status restoration. | `store/persistence.rs` |
| Recovered-PR validation | `store.rs:1433-1636` | Collects candidates and checks GitHub PR state during recovery. | `store/recovered_prs.rs` |
| State transitions | `store.rs:1637-1862` | Applies ordinary/terminal mutations, persists, rolls back optimistic cache state, and compares timestamps. | `store/transitions.rs` |
| Existing tests | `crates/harness-server/src/task_runner/store_tests.rs` | Exercises the root store module as a path-based test module. | Remains in place and continues to access the same facade. |

## Current System

`task_runner/store.rs` is both the public facade and implementation owner for
nearly every task lifecycle concern. `task_runner/mod.rs` privately imports
the free transition functions and publicly re-exports the four stable types.
Rust permits inherent `impl TaskStore` blocks in child modules, so the
implementation can be split without a trait, wrapper, or caller migration.

The store combines a PostgreSQL `TaskDb` with a live `DashMap` cache,
per-task persistence locks, stream senders, abort handles, runtime-host lease
state, rate-limit state, and an event log. Movement must therefore preserve
whole functions and their existing operation order.

## Proposed Design

### 1. Keep a stable root facade

Keep in `task_runner/store.rs`:

- child-module declarations and narrow internal re-exports;
- `TaskStore` with exactly the same fields and field types;
- `TaskSummaryFilter`, `TaskSummaryPageCursor`, and `TerminalTransition` at the
  same Rust path;
- shared constants and genuinely cross-cutting private records only when their
  placement produces the clearest dependency direction;
- the existing path-based `store_tests` declaration.

The root may re-export moved free functions privately so `task_runner/mod.rs`
continues to import `store::{mark_terminal_once, mutate_and_persist,
update_status}` unchanged. No compatibility alias is added.

### 2. Assign one owner per responsibility

| Module | Exact ownership |
| --- | --- |
| `startup.rs` | `open*`, `from_task_db*`, startup replay/cache hydration/recovery, and test schema/store-key accessors when required by constructor state |
| `queries.rs` | direct/cache-fallback reads, existence/dependency/terminal queries, duplicate detection, summary filtering/paging, all-status and external-status reads, and summary cursor helpers |
| `runtime_hosts.rs` | runtime-host active-lease count, claim, individual release, and bulk release |
| `projections.rs` | latest PR URL, project/sibling/child projections, dashboard counts, completion rates, and LLM metric inputs |
| `runtime_control.rs` | task-stream registration/subscription/publication/closure, abort-handle lifecycle, and rate-limit wait/update |
| `persistence.rs` | artifacts, prompts, event-log writes, task insertion/persistence, checkpoints, orphan queries, external-ID repair, and status restoration |
| `recovered_prs.rs` | recovered task validation, candidate collection, GitHub client/token variants, and PR-state checking helpers |
| `transitions.rs` | `update_status`, `TerminalTransition` implementation, `mark_terminal_once`, `mutate_and_persist`, optimistic rollback comparison, and timestamp ordering helper |

If formatted size or dependency direction requires a further split, it must
use a cohesive subset of this table and preserve one owner per symbol. No
touched non-exempt production file may exceed 800 lines.

### 3. Move complete functions mechanically

Move function bodies with their SQL strings, bind sequences, lock acquisition,
cache mutation, event-log calls, stream actions, callbacks, warnings/errors,
and return values intact. Allowed changes are limited to module declarations,
imports, `super` paths, and the narrowest visibility needed by siblings.

Shared implementation visibility rules:

- private when used by one module;
- `pub(super)` when used only by sibling store modules;
- retain existing `pub(crate)` or `pub` only for existing callers;
- never make an existing private helper public outside `task_runner::store`.

### 4. Preserve startup and transition ordering

Construction continues to open the same `TaskDb`, create the same store
fields, replay the same event log, hydrate the same cache, and invoke the same
startup recovery path in the same order.

Transition functions continue to:

1. acquire the same per-task lock;
2. read and mutate the same cached state;
3. apply the same terminal/single-winner predicate;
4. persist through the same `TaskDb` operation;
5. restore cache state on the same persistence failure condition;
6. emit the same logs, stream events, closures, and callbacks in the same order.

### 5. Inventory and size gates

Before extraction, record every root public type, inherent method, free
function, helper, and production line count at the implementation base SHA.
After formatting, compare the inventory across the root and all new child
modules. Each entry must exist exactly once and every existing caller must
compile through the unchanged facade.

### 6. Verification database

Create a dedicated disposable database named
`harness_codex_gh1646_test` on the existing local PostgreSQL server at
`127.0.0.1:55432`. Set `HARNESS_DATABASE_URL` only for verification. Cap the
application pool with `HARNESS_DATABASE_POOL_MAX_CONNECTIONS=8`, and run the
full database-backed workspace suite with `--test-threads=1` to avoid local
connection saturation. Do not modify or stop the PostgreSQL service, and drop
only this disposable database after verification.

## Data Flow

There is no data-flow change:

```text
caller
  -> TaskStore method or transition function (same Rust path)
  -> same cache / lock / sqlx / event-log / stream / callback sequence
  -> same PostgreSQL schema and persisted task representation
  -> same result or propagated error
```

Only the source module containing each implementation changes.

## Product-to-Test Mapping

| Invariant | Implementation area | Verification |
| --- | --- | --- |
| B-001 API paths/signatures | root facade and extracted inherent impls | `cargo check -p harness-server --all-targets`; compile all current call sites without aliases |
| B-002 SQL/cache/locks/leases | query, runtime-host, persistence, transition modules | movement-aware diff plus isolated-DB `cargo test -p harness-server --lib` |
| B-003 startup sequence | `startup.rs` | existing open/recovery/replay server tests and review of unchanged whole-function bodies |
| B-004 error behavior | all extracted modules | existing negative-path tests, workspace Clippy, and review for no new fallback/default/panic |
| B-005 concurrency/single winner | runtime-host and transition modules | isolated-DB server tests covering claims, terminalization, and persistence rollback |
| B-006 side-effect order | runtime control, persistence, recovery, transitions | existing stream/recovery/callback tests plus whole-function movement review |
| B-007 complete inventory | root plus `task_runner/store/*.rs` | compare pre/post `rg` symbol inventories and compile every existing caller |
| B-008 file-size ceiling | every touched production store file | after format, run `wc -l` over root and child modules and fail any count above 800 |
| B-009 no schema/model/dependency change | manifests, lockfile, migrations, task/state types | path-limited `git diff origin/main` must be empty |
| B-010 complete evidence | local handoff and exact-head PR evidence | all named commands plus `checks/check_workflow.py`, implementation comparison, and `checks/pr_gate.py` pass |

## Risks

- Concurrency: moving only part of a mutation could change lock lifetime.
  Mitigation: move whole functions and review lock/cache/persist order.
- Compatibility: moving stable types or free functions could change import
  paths. Mitigation: keep types in the facade and use narrow internal
  re-exports for moved functions.
- Recovery: a module split could change startup ordering or warning behavior.
  Mitigation: preserve constructor bodies and run recovery tests with a real
  PostgreSQL database.
- Visibility: siblings may tempt unnecessary `pub(crate)`. Mitigation: prefer
  private or `pub(super)` and verify no new public surface.
- Verification infrastructure: parallel server/workspace tests can exhaust
  local PostgreSQL connections. Mitigation: pool cap eight and serial final
  workspace tests; infrastructure reruns must never conceal assertions.
- Maintenance: excessive micro-modules can hurt navigation. Mitigation: the
  ownership table uses lifecycle responsibilities rather than per-method files.

## Alternatives Considered

1. Leave the file because the legacy layer will eventually disappear.
   Rejected: it is still active, exceeds a hard ceiling, and receives
   correctness-sensitive maintenance.
2. Move only types or constants. Rejected: the mixed implementation remains
   well above 800 lines.
3. Introduce traits or multiple store objects. Rejected: that changes the
   architecture and behavior surface without evidence it is needed.
4. Combine the extraction with SQL or cache optimizations. Rejected: behavioral
   changes would prevent a clean movement proof and increase regression risk.

## Test Plan

- [ ] `cargo fmt --all -- --check`.
- [ ] `cargo check -p harness-server --all-targets`.
- [ ] `HARNESS_DATABASE_URL=<isolated-url> HARNESS_DATABASE_POOL_MAX_CONNECTIONS=8 cargo test -p harness-server --lib -- --test-threads=1`.
- [ ] `cargo clippy --workspace --all-targets -- -D warnings`.
- [ ] `HARNESS_DATABASE_URL=<isolated-url> HARNESS_DATABASE_POOL_MAX_CONNECTIONS=8 cargo test --workspace -- --test-threads=1`.
- [ ] `python3 checks/check_workflow.py --repo .`.
- [ ] `python3 checks/check_workflow.py --repo . --spec-dir specs/GH1646`.
- [ ] Pre/post symbol inventory, formatted line-count, no-manifest/model/migration,
      and movement-aware diff gates from the mapping table.

## Rollback Plan

Revert the implementation PR. No schema, migration, dependency, task model, or
persisted-format rollback is required. Before merge, restore moved functions to
the root if a regression cannot be resolved mechanically; do not add duplicate
compatibility paths or weaken a test.
