# Review Integrity — Analysis Report

> Linked issue: GH-1767
> Date: 2026-07-26
> Scope: the code-review layer of the harness — cross-agent review, review
> degradation behavior, and the fate of the external-bot review escalation
> ladder after the legacy `task_executor` removal (#1725).
> Method: read-only inspection at commit `81c78255`. All file:line references
> were re-verified at that commit.

## 1. The live review chain (what runs today)

A `github_issue_pr` workflow passes through four review-relevant gates before
merge:

1. **`local_review_gate`** — the runtime enqueues a `run_local_review`
   activity. An agent reviews the PR diff and reports
   `LocalReviewPassed / LocalReviewChangesRequested / LocalReviewBlocked`
   signals (`crates/harness-workflow/src/runtime/pr_feedback.rs`). The
   reviewer's verdict arrives through the agent-authored
   `harness-activity-result` block.
2. **`awaiting_feedback`** — a child `pr_feedback` workflow runs
   `inspect_pr_feedback`, which is **server-owned**: the server queries GitHub
   GraphQL itself (`crates/harness-server/src/github_pr_snapshot.rs`) and no
   LLM is involved in observing review threads, check status, or mergeability.
3. **`quality_gate_pending`** — a child `quality_gate` workflow runs
   validation commands from `WORKFLOW.md` via a `run_quality_gate` activity.
4. **`ready_to_merge` → `merging`** — the merge is executed server-side
   (`crates/harness-server/src/workflow_runtime_worker/server_merge.rs`),
   re-snapshotting before the merge call and verifying
   `expected_head_sha_for_merge` (`server_merge.rs:136-151`), under the
   deterministic policy in `crates/harness-server/src/http/auto_merge.rs`
   (`status_check_rollup_state == SUCCESS`, `review_decision == APPROVED`,
   draft = false, review threads resolved, base-ref match).

### Structural strengths worth preserving

- **The merge gate is deterministic and server-fed.** Every predicate is
  evaluated against a server-collected GraphQL snapshot, never against agent
  prose.
- **No self-approval by construction.** The harness never submits reviews:
  the only `pulls/*/reviews` references in `crates/` are read-only `gh api`
  GETs embedded in agent prompts (`prompts/pr.rs:25,204`;
  `prompts/review.rs:94,124`), so the `review_decision == APPROVED`
  requirement can only be satisfied by an external reviewer.
- **Head identity is pinned at merge.** A stale snapshot cannot merge a PR
  whose head moved (`server_merge.rs:147-151`).

## 2. Defect G — cross-review fails open on protocol violation

`crates/harness-server/src/handlers/cross_review.rs:224-239`:

```rust
consensus_issues = extract_tagged(&challenger_review, "CONFIRMED")
    .into_iter()
    .chain(extract_tagged(&challenger_review, "MISSED"))
    .collect();

if consensus_issues.is_empty() {
    ...
    final_verdict: "APPROVED".to_string(),
```

`extract_tagged` matches line prefixes (`CONFIRMED:` / `MISSED:` /
`FALSE-POSITIVE:`). Any challenger output that does not use the tag protocol —
an empty string, a refusal, a quota-exhaustion error message that reached
stdout, or ordinary prose describing real problems — produces an empty
`consensus_issues` set and therefore the verdict `APPROVED`.

The now-deleted sibling implementation made the opposite (correct) choice: the
legacy `task_executor/agent_review.rs:430-454` treated reviewer output with
neither `APPROVED` nor `ISSUE:` lines as a **protocol failure that fails the
task**, precisely to avoid crediting no-op review passes. The same repository
therefore shipped both polarities of the same decision; the surviving one is
the unsafe one.

Failure scenario: the challenger CLI hits a usage limit and prints a
one-line quota message. `extract_tagged` finds no tags, the function returns
`final_verdict: "APPROVED"` with `contested_issues: []`, and the caller
records a clean cross-review that never happened.

## 3. Defect H — silent single-model degradation

Two variants:

1. **No challenger registered.**
   `cross_review.rs:135-148`: when `challenger` is `None` the function returns
   a result whose only distinguishing marks are `challenger_review: ""` and
   `rounds: 1`. The doc comment calls this "graceful degradation", but nothing
   in `CrossReviewResult` names it: there is no `degraded: bool`, no
   `challenger_id`, and `final_verdict` is the same `"APPROVED"` string the
   full protocol produces. Downstream consumers cannot distinguish
   "two models agreed" from "one model reviewed itself" without string-sniffing
   an empty field.
2. **Primary and challenger are the same model.**
   `cross_review.rs:59-64`: `primary` is `agent_registry.default_agent()` and
   `challenger` is `agent_registry.get("codex")`. If the default agent *is*
   codex, the same model plays both roles. No identity comparison exists, so
   the result is presented as adversarial cross-review while being a
   self-conversation.

The value of cross-review is exactly its independence assumption; a result
object that cannot express "independence was not achieved" converts every
degraded run into silent overconfidence.

## 4. Defect: the external-review escalation ladder was deleted without a port

History:

- The legacy `task_executor/` layer contained the external-bot review driver:
  `ReviewBotKey::{Gemini, Codex}` reviewer identities, quota-exhaustion
  detection (`is_quota_exhausted`, "codex usage limits have been reached"),
  the `ReviewFallbackTier::{A, B, C}` /
  `ReviewFallbackTrigger::{GeminiQuota, CodexQuota, AllBotsQuota, Silence}`
  graduation ladder, Jaccard-similarity repeat-issue detection
  (`agent_review.rs`), and head-SHA-bound local-review approval verification
  (`local_review_completion.rs`: approvals carried an `approved_review_sha`,
  re-verified against the live head before completion).
- Commit `af5bcd26` (`refactor(server): remove legacy task_executor execution
  path (GH-1434 T008)`, PR #1725) deleted the entire layer: 72 files,
  **−19,436 lines**.

What survived, verified at `81c78255`:

- The **data model** survived: `ReviewFallbackSnapshot`, `ReviewFallbackTier`,
  `ReviewFallbackTrigger` in
  `crates/harness-workflow/src/issue_lifecycle.rs:101-126`, plus the store
  method `record_ready_to_merge_with_fallback`
  (`issue_workflow_store.rs:411`).
- The **producers did not**: the only non-test constructor of
  `ReviewFallbackSnapshot` is inside `issue_lifecycle.rs` itself; grep over
  `crates/harness-server/src/workflow_runtime_worker/`,
  `workflow_runtime_pr_feedback/`, and `crates/harness-workflow/src/runtime/`
  finds **zero** references to fallback tiers or any `ReviewFallbackSnapshot`
  producer. Nothing on the live path ever creates a Tier-A/B/C snapshot.
  Quota *detection* does exist on the runtime path — but it is derived from
  agent-reported prose, not server observation:
  `turn_error_is_non_retryable_agent_limit` calls
  `is_quota_failure_message` over turn error text
  (`workflow_runtime_worker/activity_result.rs:462`), and the status contract
  emits a `text:review_quota_blocker` signal from substring matching on the
  agent's own summary (`activity_status_contract.rs:204-205`). Neither feeds
  any escalation; both strengthen the case for B-009's server-derived
  triggers, since today's only quota signals originate in the very
  agent-authored text the escalation contract must not trust.

Consequences:

1. **No review-bot escalation exists anywhere.** When the external reviewer
   (e.g. the Gemini bot the PR workflow depends on) is silent or
   quota-exhausted, the live system has no fallback: the workflow simply
   waits in `awaiting_feedback` / fails the `review_decision == APPROVED`
   merge predicate indefinitely. The legacy ladder existed precisely because
   bot quota exhaustion is a routine operational event (documented in the
   2026-07 operations audit: Gemini/Codex bots hitting quota blocked merge
   gates across repos).
2. **Head-bound local-review approval verification is gone.** The runtime's
   head pinning now exists only at the merge step (`server_merge.rs`); the
   local review gate accepts a `LocalReviewPassed` signal without proof of
   *which commit* was reviewed. A review of head N followed by a push of
   head N+1 still passes the local gate.
3. **Vestigial model invites false confidence.** `GH-1715`'s transition
   contract carefully specifies Tier-C fallback snapshot semantics
   (`Mergeable` event, `review_fallback` preservation) for a producer that no
   longer exists. Readers of `issue_lifecycle.rs` and its tests reasonably —
   and wrongly — conclude the ladder is live.

## 5. Port-or-delete analysis

Two coherent end states:

**Option A — port the escalation ladder into the workflow runtime.**
Reintroduce external-review escalation as runtime concepts: a
`review_escalation` state or signal family on the `pr_feedback` child
workflow, driven by the server-owned snapshot (bot silence = no review events
after N sweeps; quota = bot comment matching known quota patterns), graduating
review authority Tier A (external bot) → Tier B (alternate bot) → Tier C
(harness-internal independent agent review with distinct-model guarantee +
operator gate). Keeps the merge predicate `review_decision == APPROVED`
intact for Tiers A/B and substitutes an explicit operator-visible fallback
receipt for Tier C.

**Option B — delete the vestigial model.**
Remove `ReviewFallbackSnapshot`/`Tier`/`Trigger` and
`record_ready_to_merge_with_fallback`, accept "wait for external review or
operator intervention" as the only behavior, and document it.

**Recommendation: Option A, in reduced form.** Operational history shows bot
quota exhaustion and bot silence are weekly events, not corner cases; without
escalation, every such event is an indefinite stall that pages an operator.
The reduced port keeps the ladder's *decision structure* (trigger taxonomy +
tier graduation + durable snapshot) but re-grounds every trigger in the
server-owned GraphQL snapshot instead of legacy prose parsing. The vestigial
data model should be reused, not duplicated: it already has validated
transition semantics from GH-1715.

Independent of A/B, defects G and H are unconditional fixes: cross-review must
fail closed on protocol violation, and degraded runs must say so in the
result.

## 6. Recommended contract (summary)

1. Challenger output containing no protocol tags is a **protocol failure**:
   the round is invalid, and cross-review returns an error or a
   `PROTOCOL_FAILURE` verdict — never `APPROVED`.
2. `CrossReviewResult` carries `mode` (`cross_model` | `single_model_degraded`),
   `primary_agent_id`, `challenger_agent_id`. Same-identity primary/challenger
   is refused or reported as degraded, never as cross-review.
3. External-review escalation is a runtime-owned ladder driven by server
   snapshots, with each tier transition recorded as a durable, operator-visible
   event; Tier-C completion requires an operator gate.
4. The legacy fallback data model either gains a live producer (Option A) or
   is deleted with its store method (Option B); it does not remain
   producer-less.

See `specs/GH1767/product.md` and `specs/GH1767/tech.md` for the binding
behavior contract.
