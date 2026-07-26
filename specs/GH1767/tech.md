# Tech Spec

## Linked Issue

GH-1767

## Context

Three review-integrity defects verified at commit `81c78255` (full analysis:
`docs/references/review-integrity.md`):

1. `crates/harness-server/src/handlers/cross_review.rs:224-239` — challenger
   output with no `CONFIRMED:`/`MISSED:` tag lines yields an empty consensus
   set, which returns `final_verdict: "APPROVED"`. Fail-open.
2. `cross_review.rs:135-148` — absent challenger silently degrades to a
   single-model result distinguishable only by `challenger_review: ""`;
   `cross_review.rs:59-64` — `default_agent()` vs `get("codex")` can resolve
   to the same agent with no identity check.
3. PR #1725 (`af5bcd26`) deleted the legacy `task_executor` layer (−19,436
   lines) including the external-bot review escalation driver
   (`ReviewBotKey`, quota detection, `ReviewFallbackTier` graduation). The
   data model survives producer-less in
   `crates/harness-workflow/src/issue_lifecycle.rs:102-121` and
   `issue_workflow_store.rs:411`; no runtime path references bot fallback at
   all.

## Design

### D1 — Fail-closed cross-review protocol

Extend `CrossReviewResult` (`handlers/cross_review.rs:40-48`):

```rust
pub struct CrossReviewResult {
    pub mode: CrossReviewMode,          // CrossModel | SingleModelDegraded
    pub primary_agent_id: String,
    pub challenger_agent_id: Option<String>,
    pub primary_review: String,
    pub challenger_review: String,
    pub consensus_issues: Vec<String>,
    pub contested_issues: Vec<String>,
    pub rounds: u32,
    pub final_verdict: CrossReviewVerdict,
    pub protocol_failure: Option<ProtocolFailure>, // round, excerpt (bounded)
}

pub enum CrossReviewVerdict {
    Approved,            // cross_model, tags parsed, no consensus issues
    ApprovedDegraded,    // single_model_degraded, no issues
    NotConverged,
    ProtocolFailure,
}
```

Rules in `run_cross_review_with_context`:

- After each challenger round, if
  `extract_tagged(reply, "CONFIRMED") ∪ extract_tagged(reply, "MISSED") ∪
  extract_tagged(reply, "FALSE-POSITIVE")` is empty **and** the reply contains
  no tag prefix at all, return `ProtocolFailure { round, excerpt }` with
  verdict `ProtocolFailure`. A reply with only `FALSE-POSITIVE:` tags remains
  a valid approving round (protocol was followed).
- The no-challenger branch (`:135-148`) sets
  `mode = SingleModelDegraded`, `challenger_agent_id = None`, and maps an
  empty issue set to `ApprovedDegraded` (never `Approved`).
- Identity guard at `cross_review(...)` (`:59-64`): resolve both agents, and
  if `primary.id() == challenger.id()` treat as no-challenger. `CodeAgent`
  gains an `fn id(&self) -> &str` (registry key + model) — additive trait
  method with a default implementation returning the registry key.

Serialization: `final_verdict` continues to serialize as an uppercase string
(`"APPROVED"`, `"APPROVED_DEGRADED"`, `"NOT_CONVERGED"`,
`"PROTOCOL_FAILURE"`) for RPC compatibility; `mode` and agent ids are new
additive fields.

### D2 — Runtime external-review escalation ladder

New module `crates/harness-workflow/src/runtime/review_escalation.rs` plus a
server driver hooked into the existing `pr_feedback` sweep (the same loop
that runs server-owned `inspect_pr_feedback`).

Trigger evaluation (pure function over the server GraphQL snapshot +
persisted fallback state):

```rust
pub enum EscalationTrigger {
    ReviewerQuotaExhausted { bot: String },  // bot comment matches quota patterns
    ReviewerSilence { waited_secs: u64 },    // no review event since PR head push, past threshold
}

pub fn evaluate_escalation(
    snapshot: &PrSnapshot,
    current: Option<&ReviewFallbackSnapshot>,
    policy: &EscalationPolicy,
    now: DateTime<Utc>,
) -> Option<EscalationStep>; // None | Some(ToTierB{..}) | Some(ToTierC{..})
```

- Inputs are exclusively server-collected (`github_pr_snapshot.rs` output:
  review events, comment bodies + authors, timestamps, head push time).
  Agent activity results are not consulted (B-009).
- Quota patterns are configuration (`WORKFLOW.md` `review_escalation.bots[]`
  with `name`, `quota_patterns[]`), not hardcoded prose heuristics.
- Tier order is monotonic; `evaluate_escalation` returns `None` when the
  persisted tier already covers the observed trigger (idempotent under
  re-observation, satisfying GH-1715 first-snapshot-wins).

Persistence reuses the existing model: the driver applies the `Mergeable` /
fallback path through `IssueWorkflowStore::record_ready_to_merge_with_fallback`
for legacy rows and stores the snapshot in `workflow.data.review_fallback`
for runtime instances (same shape, one serializer). Each transition also
emits a `WorkflowEvent` so escalation history is auditable in
`workflow_events`.

Tier C: enqueue an internal independent review activity that runs under D1's
cross-review with `mode` required to be `CrossModel` (distinct model enforced
by the identity guard); on completion the workflow enters
`ready_to_merge`-adjacent **operator gate** (`RequestOperatorAttention`
command), never `merging` directly. The `auto_merge.rs` predicate set is
untouched: without an external `review_decision == APPROVED`, server merge
still refuses; Tier C exists to hand an operator a reviewed, evidence-backed
PR, not to merge it.

Configuration:

```yaml
review_escalation:
  enabled: false          # default off for one release
  silence_threshold_secs: 86400
  bots:
    - name: gemini
      quota_patterns: ["exceeded your current quota", ...]
    - name: codex
      quota_patterns: ["usage limits have been reached", ...]
```

`enabled: false` + observed trigger → emit `RequestOperatorAttention` with a
distinct reason code (B-010).

### D3 — No producer-less model

This spec's implementation lands the D2 producer. If D2 is descoped, the
same change set must instead delete `ReviewFallbackSnapshot`,
`ReviewFallbackTier`, `ReviewFallbackTrigger`,
`record_ready_to_merge_with_fallback`, and their GH-1715 contract rows —
tracked as an explicit either/or acceptance criterion, so the vestigial state
cannot persist.

## Affected Surface

| Area | Files |
| --- | --- |
| Cross-review protocol | `crates/harness-server/src/handlers/cross_review.rs`, `crates/harness-core/src/prompts/cross_review.rs` (challenger prompt states the fail-closed tag contract), `crates/harness-core/src/agent.rs` (additive `CodeAgent::id`) |
| Escalation ladder | new `crates/harness-workflow/src/runtime/review_escalation.rs`, `crates/harness-server/src/workflow_runtime_pr_feedback/` (sweep driver hook), `crates/harness-workflow/src/issue_lifecycle.rs` (reused model, no shape change) |
| Config | `WORKFLOW.md` schema (`review_escalation` block), `crates/harness-core/src/config/` |
| Docs | `docs/references/review-integrity.md` (this analysis) |

## Test Plan

1. **Protocol table test** (`cross_review` unit): reply matrix {empty,
   whitespace, refusal prose, quota message, untagged issue prose,
   only FALSE-POSITIVE tags, mixed tags + trailing error line} × round
   position {1, mid, final} → expected verdict per B-001/B-002; snapshot the
   bounded excerpt.
2. **Identity/degradation tests**: no challenger; same registry key; same
   model under different keys → `SingleModelDegraded` + `ApprovedDegraded`;
   distinct models → `CrossModel`.
3. **Escalation pure-function table test**: snapshot fixtures for quota
   comment, silence below/above threshold, recovery-before-threshold,
   both-bots-exhausted, existing Tier-B snapshot, disabled policy →
   expected `EscalationStep`/`None`/operator-signal.
4. **Runtime integration**: Tier transition persists exactly one snapshot and
   one `WorkflowEvent` under repeated sweeps; Tier-C completion cannot reach
   `merging` without the operator gate (reuse `server_merge.rs` stale-head
   test harness); agent activity result attempting to set
   `review_fallback` is rejected by the existing
   `agent_must_not_edit_workflow_tables` contract.
5. **Compatibility**: existing serialized `CrossReviewResult` consumers
   (RPC round-trip test) tolerate the additive fields; legacy lifecycle rows
   with persisted fallback snapshots load unchanged (GH-1715 tests stay
   green).

## Risks

- **Verdict namespace change**: consumers switching on the exact string
  `"APPROVED"` will now also see `"APPROVED_DEGRADED"` and
  `"PROTOCOL_FAILURE"`. This is the point — but it must be called out in the
  changelog; grep shows the only in-repo consumer is the RPC handler and
  tests.
- **Quota-pattern drift**: bot quota messages change wording; patterns are
  config, and a miss degrades to the silence trigger (bounded delay, not a
  stall).
- **False silence triggers** on repos with genuinely slow human review:
  threshold is per-config; the default (24h) errs long, and Tier B is another
  bot, not a merge shortcut.

## Effort

- D1 (fail-closed + identity + degradation marking): ~1 day incl. tests.
- D2 (escalation ladder, config, driver, integration tests): ~3–4 days.
- D3 rides on D2 (or is a half-day deletion if D2 is descoped).
