# GH1656 Tech Spec: Declarative Intake Bindings

Product spec: `specs/GH1656/product.md`
GitHub issue: `#1656`
Depends on: GH-1609 (merged). Related: GH-1652, GH-1607.

## Codebase Context (verified anchors, origin/main)

- Intake trait and orchestrator:
  `crates/harness-server/src/intake/mod.rs:51-69` (`IntakeSource`:
  `poll() -> Vec<IncomingIssue>`, `mark_dispatched`, `unmark_dispatched`,
  `on_task_complete`), `mod.rs:16-39` (`IncomingIssue` with `source`,
  `external_id`, `labels`, `author_trust_class`, `project_root`),
  `mod.rs:83-145` (orchestrator start + poll loop with maintenance
  window), `mod.rs:145+` (`poll_tick`: group by repo → coverage gate →
  dispatch), hardwired destination note at `mod.rs:84-86`.
- Coverage gate (dedupe exemplar):
  `crates/harness-server/src/intake/github_coverage_gate.rs:14-103`
  (`issue_lifecycle_state_is_covered`, `runtime_issue_state_is_covered`,
  `record_issue_remote_fact_snapshot`, `check_github_issue_coverage`).
- Declarative submission (reuse point):
  `crates/harness-server/src/workflow_runtime_submission/declarative.rs:27`
  (`record_declarative_submission`), `declarative.rs:51`
  (`resolve_declarative_definition_for_project`).
- Declarative config + registration:
  `crates/harness-core/src/config/workflow.rs:104-115`
  (`WorkflowDefinitionPolicy`), startup registration + freeze
  `crates/harness-server/src/server.rs:110-127`.
- Sources: `intake/github_issues.rs` (REST poll + webhook modes),
  `intake/feishu.rs`, `intake/direct_dispatch.rs`; source configs in
  `crates/harness-core/src/config/intake.rs:241` (`GitHubIntakeConfig`),
  `:439` (`FeishuIntakeConfig`), `:796` (`IntakeConfig`).
- Instance store dedupe query surface:
  `list_nonterminal_instances_by_definition` (used by dashboards; see
  `handlers/dashboard_active_counts.rs:106-111`).

## Proposed Design

### Config schema (harness-core, `config/workflow.rs`)

```rust
pub struct WorkflowDefinitionIntakePolicy {
    pub source: String,                 // registered intake source name (B-003)
    pub filter: IntakeFilterPolicy,
    pub max_active_instances: u32,      // >= 1 (B-002, B-008)
}

pub struct IntakeFilterPolicy {
    pub labels: Vec<String>,            // non-empty; issue must carry ALL (B-002)
    #[serde(default)]
    pub exclude_labels: Vec<String>,    // issue must carry NONE
}
```

- New optional field on `WorkflowDefinitionPolicy`:
  `pub intake: Option<WorkflowDefinitionIntakePolicy>` with
  `#[serde(default)]` (B-001). Rides the existing atomic `definition`
  merge — no new merge rules.
- Match semantics: `issue.source == binding.source` (source names as
  registered, e.g. "github", "feishu") AND all `filter.labels` present
  AND no `exclude_labels` present. Label matching is exact and
  case-sensitive, consistent with `IncomingIssue.labels` as delivered by
  sources. Nothing GitHub-specific (B-003).

### Startup validation and binding registry (harness-server)

Extend `register_declarative_workflow_definitions`
(`server.rs:110-127`): after building each definition, if
`policy.intake` is present, validate fail-closed (B-002):

- `source` names a source that will be registered and enabled per
  `IntakeConfig` (github/feishu/dashboard as configured);
- `filter.labels` non-empty, no empty strings, no duplicate labels;
- `max_active_instances >= 1`.

Valid bindings collect into an `IntakeBindingRegistry` (project-scoped:
`project_id -> Vec<Binding>`, each binding carrying definition id +
pinned identity) stored on `AppState` alongside existing intake state.
Built once at startup, immutable afterwards — same freeze discipline as
the definition registry.

### Routing in `poll_tick` (intake/mod.rs)

Insert a routing step per issue, before the existing
github-coverage-gate/dispatch path:

1. Resolve the issue's project (the existing `project_root` →
   `resolve_canonical_project` flow at `mod.rs:184-196`).
2. Look up that project's bindings; collect matches
   (source + labels). No match → existing path unchanged (B-006).
3. Multiple matches → sort by definition id, take the smallest, record
   the tie in the audit reason (B-004).
4. Dedupe check: query nonterminal instances of the target definition
   whose subject `external_id` equals the issue's. Live instance →
   audited suppression, `mark_dispatched` NOT called (a terminal
   instance later re-opens eligibility, B-007). Implementation note: add
   a store lookup keyed by (definition_id, subject external_id) — an
   indexed query or a filtered scan over
   `list_nonterminal_instances_by_definition`, whichever the store
   supports cheaply; the invariant is the key, not the query plan.
5. Cap check: count nonterminal instances for the definition; at
   `max_active_instances`, audited skip, no `mark_dispatched`, issue
   naturally retried next tick (B-008).
6. Dispatch: `resolve_declarative_definition_for_project` +
   `record_declarative_submission` with subject metadata (external_id,
   identifier, repo, url, title) and `author_trust_class` (B-005,
   B-011). On success, `source.mark_dispatched(external_id, ...)`; on
   failure, `unmark_dispatched` — mirroring the existing github path's
   failure handling.
7. Cross-project mismatch (binding exists in another project only):
   falls out naturally from step 2's project-scoped lookup; audited via
   the routing record (B-012).

Audit records (B-009) reuse the observe event store with a new
`intake_routing` event kind: fields issue identifier, source, matched
definition (or none), outcome (routed / suppressed_live /
skipped_at_cap / unmatched), tie list when applicable.

### What does NOT change

- github_issue_pr coverage gate and dispatch for unmatched issues.
- Sources themselves — no `IntakeSource` trait change (B-010: adding a
  Linear source later is purely additive and needs no binding-schema
  work, satisfying the source-pluggability requirement).
- Declarative runtime semantics (B-011): intake-born instances are
  created by the same submission function operators use.

## Edge Cases

- Webhook-mode GitHub intake (poller off, `webhook.rs:179` note): the
  routing step lives in the shared dispatch path used by both poll and
  webhook deliveries, so bindings work in webhook-only mode too; the
  spec's tick-retry semantics for cap-skips degrade to
  next-delivery-retry there (documented in the audit reason).
- An issue matching a binding whose label also matches github_issue_pr
  triggers (e.g. `force-execute`): binding wins (routing is exclusive,
  B-006); the audit record makes the diversion visible.
- Source emitting duplicate external_ids in one tick: first routes,
  second hits the dedupe check within the same tick (store-backed, not
  in-memory only).
- `max_active_instances` reduced across a restart below the current live
  count: no instances are killed; new dispatch stays skipped until the
  count drains below the cap.

## Migration / Compatibility

- `intake` is optional with `#[serde(default)]`; existing WORKFLOW.md
  files parse unchanged (B-001). No schema migration; the audit event
  kind is additive.
- Bindings ride the definition content hash: adding/changing `intake`
  changes `definition_version` for NEW instances only — in-flight
  instances are unaffected (pinning semantics from GH-1609 carry over).

## Verification Plan

- harness-core: schema parse + validation unit tests (empty labels,
  zero cap, duplicates); atomic-merge regression with an `intake` block.
- harness-server unit: binding matcher (source + labels + excludes,
  case sensitivity, non-GitHub source fixture — B-003); tie-break
  ordering + audit reason (B-004).
- Integration (stub runtime + fixture source): route → instance created
  with subject + trust class (B-005); dedupe suppression while live and
  re-eligibility after terminal (B-007); cap skip + next-tick retry
  (B-008); unmatched issue still lands in github_issue_pr (B-001/B-006);
  cross-project mismatch audited (B-012).
- Full gates: `cargo check --workspace --all-targets`,
  `cargo test -p harness-core -p harness-server`,
  `cargo clippy --workspace --all-targets -- -D warnings`,
  `cargo fmt --all -- --check`.

## Rollback Plan

Feature is inert without `intake` blocks (B-001). Rollback = remove the
blocks (instances already created finish normally under pinned
versions). Code revert removes the config field, binding registry, and
the routing step; the unmatched path is the existing code, untouched.

## Product-to-Test Mapping

| Invariant | Implementation area | Verification |
|---|---|---|
| B-001 | optional field + no-binding fast path | parse compat test + intake integration test without bindings (byte-identical dispatch) |
| B-002 | startup validation in server.rs registration | `cargo test -p harness-server` boot-validation tests |
| B-003 | matcher over normalized IncomingIssue | matcher unit tests incl. non-GitHub fixture source |
| B-004 | sorted multi-match + audit reason | matcher unit test |
| B-005 | submission call with subject + trust | integration test asserting instance fields |
| B-006 | exclusive routing, unmatched fall-through | integration test (matched never reaches github path; unmatched unchanged) |
| B-007 | store-backed (definition_id, external_id) dedupe | integration test live-suppress / terminal-reopen |
| B-008 | cap check + skip audit | integration test at cap + next-tick retry |
| B-009 | `intake_routing` audit events | assertions on event store records per outcome |
| B-010 | no new external calls | review gate: diff adds no HTTP/tracker client |
| B-011 | reuse of record_declarative_submission | integration test: cancel/recovery work on intake-born instance |
| B-012 | project-scoped binding lookup | integration test with binding in another project |
