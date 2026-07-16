# GH1652 Tech Spec: Registry-Driven Definition Enumeration

Product spec: `specs/GH1652/product.md`
GitHub issue: `#1652`
Related: GH-1609 (declarative definitions), GH-1608 (registry)

## Codebase Context (verified anchors, origin/main)

Static enumeration sites (in scope):

- `crates/harness-server/src/handlers/operator_monitor.rs:39-44` —
  `const WORKFLOW_DEFINITION_IDS: &[&str]` listing the four built-ins;
  iterated at `operator_monitor.rs:364`.
- `crates/harness-server/src/handlers/operator_monitor/sampling.rs:1`
  imports that const; iterates it at `sampling.rs:10` and `sampling.rs:36`.
- `crates/harness-server/src/handlers/dashboard_active_counts.rs:106-110`
  — inline array `[GITHUB_ISSUE_PR_DEFINITION_ID, PROMPT_TASK_DEFINITION_ID]`
  feeding `list_nonterminal_instances_by_definition`.
- `crates/harness-server/src/handlers/overview.rs:484-487` — same inline
  two-id array feeding the same store call.

Registry API (the replacement source):

- `crates/harness-workflow/src/runtime/state_registry.rs:385`
  (`known_workflow_definition_ids() -> Vec<String>`, free function over
  the process registry), registry method `state_registry.rs:256`
  (`known_definition_ids`). The registry is seeded with built-ins and
  frozen after startup registration
  (`crates/harness-server/src/server.rs:110-127`), so request-time reads
  are stable snapshots.

Exempt per-definition behavior (not enumeration; out of scope, B-007
exemption list):

- `crates/harness-server/src/runtime_projection.rs:295` (quality-gate
  `pending` special case) and other `definition_id == ...` behavioral
  branches in projection/worker code.

Store query used by all four sites:
`list_nonterminal_instances_by_definition(definition_id, ...)` — already
string-keyed, works for declarative ids unchanged.

## Proposed Design

### One enumeration helper

Add a small helper in `harness-server` (e.g.
`crates/harness-server/src/handlers/definition_ids.rs` or an existing
shared handlers module):

```rust
/// Definition ids for operator-facing enumeration, from the frozen
/// registry, in sorted order (B-001, B-004). Errors if the registry
/// reports no definitions (B-006) — impossible after healthy startup.
pub fn operator_definition_ids() -> anyhow::Result<Vec<String>> {
    let mut ids = harness_workflow::runtime::known_workflow_definition_ids();
    if ids.is_empty() {
        anyhow::bail!("workflow definition registry returned no definitions");
    }
    ids.sort();
    Ok(ids)
}
```

### Call-site changes

1. `operator_monitor.rs`: delete `const WORKFLOW_DEFINITION_IDS`
   (`:39-44`); `:364` iterates `operator_definition_ids()?`. The
   `sampling.rs` functions take the id slice as a parameter or call the
   helper — whichever keeps signatures smallest.
2. `dashboard_active_counts.rs:106` and `overview.rs:484`: replace the
   inline two-id arrays with the helper. Note both sites today enumerate
   only two of the four built-ins — verify against git history whether
   pr_feedback/quality_gate exclusion is intentional (child workflows may
   be deliberately excluded from "active counts" because they shadow
   their parents). If intentional, the helper gains an explicit filter
   (`is_top_level_definition`, driven by registry metadata or a
   documented predicate: built-in child definitions excluded, declarative
   definitions included) rather than an id list; the exclusion decision
   and rationale must land in the code comment and in this spec during
   implementation (B-002 vs B-003 tension is resolved by preserving
   today's built-in exclusions exactly and adding declarative ids).
3. Ordering: sort ids before iterating so payload order is deterministic
   (B-004). Today's const order is
   github_issue_pr, pr_feedback, prompt_task, quality_gate — already
   sorted alphabetically, so sorting preserves built-in output order
   (B-002).

### Determinism note

Registry content is frozen post-startup, so per-request enumeration is
stable. No caching needed; `known_workflow_definition_ids()` is a cheap
read (`Vec<String>` clone of a small set).

## Edge Cases

- Declarative definition registered with zero instances: store query
  returns empty; monitor/counts render nothing for it — same as a
  built-in with no instances (B-005).
- Multiple pinned versions of one declarative id: enumeration is by id,
  not (id, version); instances of all versions list under the one id —
  matches operator expectations.
- Deployment with no WORKFLOW.md definitions: registry returns exactly
  the four built-ins; with the preserved child-definition exclusions the
  output is byte-identical (B-002).

## Migration / Compatibility

- No persisted data, wire schema, or config changes. Payload shape is
  unchanged; only which definition ids contribute rows/counts changes,
  and only when declarative definitions exist.

## Verification Plan

- Unit: helper returns sorted ids and errors on an empty registry
  (test-only fresh registry) (B-001, B-004, B-006).
- Handler tests: register a declarative fixture definition, create a
  nonterminal (blocked) instance, assert it appears in the operator
  monitor payload, sampling output, dashboard active counts, and
  overview counts (B-003).
- Regression: built-in-only snapshot tests for monitor payload and
  active counts before/after refactor (B-002).
- Audit gate: `git grep -n "DEFINITION_IDS\|for definition_id in \["
  crates/harness-server/src` returns only the shared helper and
  documented per-definition behavioral branches (B-007).
- Full gates: `cargo test -p harness-server`,
  `cargo clippy --workspace --all-targets -- -D warnings`,
  `cargo fmt --all -- --check`.

## Rollback Plan

Read-only surface refactor; revert restores the static lists. No data or
config to unwind.

## Product-to-Test Mapping

| Invariant | Implementation area | Verification |
|---|---|---|
| B-001 | shared helper + 4 call sites | `cargo test -p harness-server handlers` + audit grep |
| B-002 | preserved exclusions + sorted order | built-in-only snapshot tests |
| B-003 | call-site swaps | declarative fixture visibility tests (monitor, sampling, counts, overview) |
| B-004 | sort in helper | helper unit test |
| B-005 | no-op for empty definitions | fixture test with zero-instance definition |
| B-006 | empty-registry error | helper unit test with fresh registry |
| B-007 | enumeration audit | grep gate in CI-visible test or review checklist |
