# Tech Spec

## Linked Issue

GH-1633

## Product Spec

`specs/GH1633/product.md`

## Codebase Context

Verified at `origin/main` commit `8729cb4f`.

| Area | Anchor | Current behavior | Relevance |
| --- | --- | --- | --- |
| Store module wiring | `crates/harness-workflow/src/runtime/store.rs:20-50` | Declares ten extracted store submodules and re-exports their public result types. | Preserve the existing facade and extend the same module pattern. |
| Public store data types | `crates/harness-workflow/src/runtime/store.rs:52-213` | Defines the store, page/count/query records, transition inputs, outcomes, and row aliases. | Public types stay at the same Rust path; private row aliases move with their query owner. |
| Store construction | `crates/harness-workflow/src/runtime/store.rs:215-249` | Opens the same workflow migrations through `PgStoreContext` and exposes the pool. | This remains in the root facade to preserve B-003. |
| Prompt payloads | `crates/harness-workflow/src/runtime/store.rs:251-294` | Upserts, reads, and deletes durable prompt payloads. | A small cohesive persistence module. |
| Workflow events | `crates/harness-workflow/src/runtime/store.rs:295-391` | Appends ordered events under an advisory lock and exposes event queries. | Event ordering and advisory-lock SQL must move unchanged. |
| Decisions and detail queries | `crates/harness-workflow/src/runtime/store.rs:392-558` | Records decisions and loads per-workflow decision/detail summaries. | Move with row conversion and preserve query ordering. |
| Command facade methods | `crates/harness-workflow/src/runtime/store.rs:559-844` | Enqueues, lists, claims, and updates workflow commands. | Integrate with the existing `store/commands.rs` owner without exceeding 800 lines. |
| Runtime-job methods | `crates/harness-workflow/src/runtime/store.rs:845-1942` | Owns job enqueue/claim, runtime events, completion, lease/state updates, cancellation, and projections. | Split by lifecycle, completion, and query responsibility; this is the largest mixed block. |
| Shared transaction helpers | `crates/harness-workflow/src/runtime/store.rs:1944-2206` | Serializes enums/JSON, loads and writes instances, appends events, records decisions, and applies inline command effects. | Give each helper one private owner and narrow sibling visibility. |
| Existing extracted modules | `crates/harness-workflow/src/runtime/store/` | Contains command, definition, instance, recovery, completion, lease, usage, and submission modules; the largest are `instances.rs` at 800 lines and `recovery.rs` at 776. | Do not push existing modules over the ceiling; create new cohesive owners where necessary. |

## Current System

`runtime/store.rs` is the public facade and also the implementation owner for
most persistence operations. Rust permits multiple inherent
`impl WorkflowRuntimeStore` blocks across sibling modules, and the repository
already uses this pattern under `runtime/store/`. Callers therefore do not
need a new trait, wrapper, or changed import path to obtain smaller files.

The current baseline compiles and passes focused clippy. Database-backed tests
cannot use the discovered global configuration because it points to inactive
`localhost:5432`; implementation verification will set
`HARNESS_DATABASE_URL` to an isolated disposable database on the available
local PostgreSQL instance.

## Proposed Design

### 1. Preserve a small root facade

Keep in `runtime/store.rs`:

- submodule declarations and public re-exports;
- `WorkflowRuntimeStore` and stable public result/query types;
- store constructors, `pool()`, and migration selection;
- only genuinely cross-cutting serialization helpers whose placement keeps
  the root below 800 lines.

Private row aliases move beside the query that consumes them. Public types do
not move to a new public Rust path.

### 2. Assign one owner per persistence responsibility

Use or create focused sibling modules with this ownership target:

| Owner | Method group |
| --- | --- |
| `store/prompt_payloads.rs` | prompt payload upsert/get/delete |
| `store/events.rs` | public workflow-event append/list/latest/batch queries |
| `store/decisions.rs` | decision writes and decision/detail summary queries |
| existing `store/commands.rs` or a focused command-query sibling | command enqueue/list/claim/status facade methods, only when the result remains at or below 800 lines |
| `store/runtime_jobs.rs` | runtime-job enqueue/claim and runtime-event writes/reads |
| `store/runtime_job_state.rs` | failure, deferral, lookup, cancellation, and state mutation operations |
| `store/runtime_job_queries.rs` | batch job/source/count/compact/turn queries and private row aliases |
| `store/activity_completion.rs` | lease-owned activity completion methods and their inline side-effect helpers |
| `store/transaction_helpers.rs` | shared transaction-local event, decision, instance, and job helpers that have multiple sibling callers |

The implementation may refine a filename when dependency direction makes a
different cohesive name clearer, but it must preserve the one-owner rule,
method inventory, and 800-line ceiling. It must not duplicate helpers merely
to avoid sibling visibility.

### 3. Preserve SQL and transaction text mechanically

Move complete functions with their query strings and bind sequences intact.
Visibility/import changes are allowed; algorithm, predicate, ordering, lock,
transaction, status, error-context, and return-shape changes are not.

Transaction-local helpers use the narrowest visibility that works:

- private when used only inside one module;
- `pub(super)` for store siblings;
- never newly public outside `runtime::store` for this change.

No compatibility alias or duplicate facade method is added. Multiple inherent
`impl WorkflowRuntimeStore` blocks continue to expose the single existing API.

### 4. Inventory and size gates

Before extraction, record:

- every public and private `WorkflowRuntimeStore` method in the root;
- every root transaction/helper function;
- root public types and re-exports;
- production file line counts.

After extraction, compare the inventory by stable function/type name. Every
entry must exist exactly once, and all call sites must compile. Run line counts
after `cargo fmt`, not before it.

### 5. Verification database

Create a dedicated disposable database on the local PostgreSQL server at
`127.0.0.1:55432`. Set `HARNESS_DATABASE_URL` only for verification commands.
The database name must clearly identify test ownership and pass the repository
disposable-database safety checks. Do not point tests at the existing
development `harness` database. Drop the disposable database after final local
verification under the maintainer's standing cleanup authorization.

## Data Flow

There is no data-flow change:

```text
caller
  -> WorkflowRuntimeStore method (same Rust API)
  -> same sqlx query / transaction / lock / bind sequence
  -> same PostgreSQL schema and persisted JSON
  -> same result or propagated error
```

The module containing a method changes; runtime values and durable state do
not.

## Product-to-Test Mapping

| Behavior invariant | Implementation area | Verification |
| --- | --- | --- |
| B-001 API paths and signatures remain stable | root facade plus all extracted inherent impls | `cargo check --workspace --all-targets`; compile every existing call site without compatibility aliases |
| B-002 SQL, locks, ordering, predicates, and transactions are preserved | extracted persistence functions | reviewer movement-aware diff plus isolated-DB `cargo test -p harness-workflow` |
| B-003 construction and migration selection are unchanged | root constructors | `cargo test -p harness-workflow plan_db::tests::shared_schema_context_uses_fixed_plan_db_schema`; inspect unchanged constructor diff |
| B-004 errors and invalid inputs do not degrade silently | extracted methods and helpers | existing negative/error tests in `cargo test -p harness-workflow`; `cargo clippy --workspace --all-targets -- -D warnings` |
| B-005 concurrency and single-winner guarantees remain | command claims, job claims, leases, completion, cancellation | isolated-DB `cargo test -p harness-workflow runtime::tests`; remote-host lease tests included |
| B-006 idempotency and deduplication remain | command/job enqueue and completion paths | isolated-DB package tests matching `dedupe`, `idempotent`, `replay`, and `concurrent` plus full package test |
| B-007 complete one-owner inventory | all moved methods/helpers and facade call sites | compare pre/post `rg` inventories; `rg -n 'impl WorkflowRuntimeStore|fn ' crates/harness-workflow/src/runtime/store.rs crates/harness-workflow/src/runtime/store/` and review for exactly one owner |
| B-008 file-size ceiling | root and all touched production modules | after format: `find crates/harness-workflow/src/runtime/store.rs crates/harness-workflow/src/runtime/store -name '*.rs' -type f -print0 | xargs -0 wc -l`; fail any touched non-exempt file above 800 |
| B-009 no dependency/schema/wire-format change | manifests, lockfile, migrations, public models | `git diff origin/main -- Cargo.toml Cargo.lock 'crates/**/Cargo.toml' crates/harness-workflow/src/runtime/store_migrations.rs crates/harness-workflow/src/runtime/model.rs` is empty |
| B-010 missing verification fails the gate | implementation handoff and PR evidence | all named commands have fresh successful output; `python3 checks/pr_gate.py --repo . --evidence <evidence.json> --json` does not report `blocked` for missing CI/review evidence |

## Risks

- Security: no authentication or secret boundary changes. Risk is accidental
  widening of helper visibility; mitigate with the narrow visibility rule and
  clippy.
- Compatibility: moving a public type could change import paths. Public types
  remain in the root facade and B-001 compiles all call sites.
- Performance: no runtime performance gain is claimed. SQL and allocation
  behavior must remain unchanged under B-002.
- Concurrency: an incomplete function move could split a transaction or alter
  a lock. Whole-function movement plus database race tests mitigate this.
- Maintenance: too many tiny modules can make navigation worse. The ownership
  table groups by durable responsibility and avoids per-method files.
- Verification: database tests can silently skip or time out under the wrong
  configuration. The dedicated `HARNESS_DATABASE_URL` and B-010 prevent a
  compile-only success claim.

## Alternatives Considered

1. Leave the file unchanged because behavior is currently correct. Rejected:
   it violates the hard file-size rule and keeps unrelated persistence changes
   in one review surface.
2. Move only public types into another file. Rejected: the 1,700-plus-line
   method implementation remains mixed and above the ceiling.
3. Introduce traits or multiple store objects. Rejected: that changes the
   architecture and public contract without evidence that a new abstraction
   is needed.
4. Combine this with SQL performance work. Rejected: it would prevent a clean
   behavior-preservation proof and expand risk.

## Test Plan

- [ ] Formatting: `cargo fmt --all -- --check`.
- [ ] Focused compile: `cargo check -p harness-workflow --all-targets`.
- [ ] Isolated database package tests:
      `HARNESS_DATABASE_URL=<isolated-url> cargo test -p harness-workflow`.
- [ ] Workspace warning gate:
      `cargo clippy --workspace --all-targets -- -D warnings`.
- [ ] Shared-behavior regression gate:
      `HARNESS_DATABASE_URL=<isolated-url> cargo test --workspace`.
- [ ] SpecRail pack and spec validation:
      `python3 checks/check_workflow.py --repo .` and
      `python3 checks/check_workflow.py --repo . --spec-dir specs/GH1633`.
- [ ] Inventory, line-count, no-manifest, and movement-aware diff checks from
      the Product-to-Test Mapping.

## Rollback Plan

Revert the implementation PR. No schema, migration, dependency, or persisted
format rollback is required. If a regression is discovered before merge, keep
the PR unmerged and restore the moved functions to the root file; do not patch
around the regression with compatibility aliases or duplicated SQL.
