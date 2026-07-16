# Tech Spec

## Linked Issue

GH-1649

## Product Spec

See `specs/GH1649/product.md`.

## Current System

`crates/harness-server/src/workflow_runtime_pr_feedback.rs` is a 1,677-line
module on base `d812d82381517fa5da77302e4429b66c3a23f126`. It contains 47
functions and 10 types and exposes the stable paths used by nine external
server source files. It already owns two child areas:

- `workflow_runtime_pr_feedback/pr_lifecycle_persist.rs` for the bounded
  lifecycle-persistence retry helper;
- `workflow_runtime_pr_feedback/tests.rs` plus nested test modules.

The remaining root combines four implementation groups:

1. public record/request/approval entry points and their context/outcome types;
2. workflow decision construction, persistence, and commit outcomes;
3. active-command queries and failed-child suppression;
4. workflow target loading, instance construction, and runtime JSON helpers.

These groups share workflow runtime types but do not need a single physical
file. The current module boundary therefore creates maintenance coupling rather
than representing one indivisible runtime operation.

## Proposed Design

Keep `workflow_runtime_pr_feedback.rs` as the stable facade for all external
callers and retain public or `pub(crate)` context/outcome types at their current
path. Extract complete function bodies, without rewriting them, into narrowly
owned child modules:

- `persistence.rs`: private `persist_*` functions, merge approval persistence,
  decision commit, and `RuntimeDecisionCommitOutcome`;
- `command_state.rs`: active-command queries, failed-child suppression, and
  command-status predicates;
- `targets.rs`: definition registration, issue/PR target loading, workflow
  instance construction, workflow IDs, runtime JSON construction, and field
  parsing helpers;
- the root facade: public entry points, stable types, small orchestration
  wrappers, constants, child wiring, and existing test wiring.

If formatted line counts show that a proposed child would exceed 800 lines,
split along the same responsibility boundaries rather than compressing or
rewriting logic. Use the narrowest `pub(super)` visibility needed for sibling
calls. Do not expose new `pub(crate)` APIs.

The existing `pr_lifecycle_persist` module remains unchanged except for a
minimal import path adjustment if Rust module ownership requires it.

## Data Flow

1. HTTP/background/task-executor callers continue calling the existing root
   module paths with the same context values.
2. Root entry points load or construct the same workflow target and build the
   same decision inputs.
3. Private child functions query the same command rows and apply the same
   dedupe/suppression predicates.
4. Persistence functions validate and commit the same decisions, instances,
   commands, evidence, events, and failure records in the same order.
5. Existing outcome enums and errors return through the unchanged root paths.

No new persistence, external call, serialization step, or fallback is added.

## Invariants and Inventory

- Capture the exact base SHA and an ordered multiset containing all function
  and type names before editing.
- After formatting, build the same multiset across the root and new child
  modules; require an exact diff with no missing or duplicate symbol.
- Compare the moved bodies against the base with a movement-aware review;
  permitted edits are module declarations, imports, visibility required for
  siblings, and qualified private paths only.
- Preserve all `WorkflowDefinition`, `WorkflowDecision`, command/evidence,
  JSON, logging, error, retry, and timestamp expressions exactly.
- Grep all external references to ensure stable caller paths remain unchanged.
- Require root and touched production child files to be at most 800 formatted
  lines.

## Affected Files

Expected production scope:

- `crates/harness-server/src/workflow_runtime_pr_feedback.rs`
- `crates/harness-server/src/workflow_runtime_pr_feedback/persistence.rs`
- `crates/harness-server/src/workflow_runtime_pr_feedback/command_state.rs`
- `crates/harness-server/src/workflow_runtime_pr_feedback/targets.rs`

`pr_lifecycle_persist.rs` may receive import-only adjustments if required.
No test, manifest, lockfile, migration, schema, model, or external caller file
is expected to change.

## Alternatives Considered

- Leave the file unchanged: rejected because it remains more than twice the
  hard production-file ceiling and couples four independently reviewable areas.
- Rewrite the lifecycle into new abstractions: rejected because it would mix
  behavior and architecture changes with the safe extraction.
- Move context/outcome types into child modules and re-export them: rejected
  unless required, because retaining them in the facade minimizes public-path
  and documentation churn.
- Split by individual workflow event: rejected because it would create many
  tiny modules with shared persistence/query helpers and obscure ownership.

## Risks

- Security: low; no auth, secret, network, or permission behavior changes. The
  diff must still be reviewed for accidental widening of visibility.
- Compatibility: medium if caller paths or serialization fields drift. Stable
  facade types and exact inventories mitigate this risk.
- Performance: low; complete bodies and query order are preserved, so no new
  allocation, query, retry, or external call is expected.
- Maintenance: low after extraction; the main risk is a misplaced private
  helper or duplicate implementation during movement.
- Persistence: medium because workflow decisions are durable. Full isolated
  PostgreSQL server tests and workspace tests are mandatory.

## Test Plan

- [ ] Baseline and final `cargo check -p harness-server --all-targets`.
- [ ] Existing PR-feedback runtime tests with an isolated PostgreSQL database.
- [ ] Existing HTTP/background/task-executor tests selected by changed call
      paths if the focused filter does not cover them.
- [ ] `cargo fmt --all -- --check` and post-format line-count gate.
- [ ] Exact function/type multiset and no-scope diff checks.
- [ ] Full `harness-server` library tests with isolated PostgreSQL.
- [ ] `cargo clippy --workspace --all-targets -- -D warnings`.
- [ ] Full database-backed workspace tests with one test thread.
- [ ] SpecRail global/packet/route checks, VibeGuard baseline/compliance, exact
      head CI, review-thread audit, and required PR gate.

## Rollback Plan

Squash-revert the implementation PR. Because the change adds no schema,
configuration, dependency, or persisted-format change, no data rollback or
operator procedure is required.
