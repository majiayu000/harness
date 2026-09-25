# PR #2008 Recovery Audit

> Historical audit date: 2026-08-27
> Current disposition: closed as superseded on 2026-09-05; baseline `121df4e4`.
> The dated findings below describe the old branch, not current main.
> Target: `feat/declarative-model-guard` at `99c7fe38`
> Baseline: `origin/main` at `93a642b3`
> Method: commit-shape analysis, cross-layer data-flow tracing, saved Codex session review, and CI evidence

## Summary

PR #2008 is closed and must not be repaired in place or merged. It contains useful design
work, but the branch combines four independently reviewable changes: a generic
model classifier, GitHub scope fact collection, built-in workflow versioning,
and merge/lease hardening. The combined change is still failing CI after 30
commits.

Since this audit was written, `main` has delivered the generic semantic
activity path as `WorkflowAgentContract`, including pinned inputs and
provenance, no-tool execution, structured verdict validation, evidence
reference validation, server-authored assessments, routing, replay, and
production dogfood. Recovery must reuse that implementation. The classifier
driver proposed by the original audit is no longer a remaining work item.

| Severity | Count | Key areas |
|---|---:|---|
| Critical | 0 | — |
| High | 5 | scope, coupling, persistence identity, verification, CI |
| Medium | 2 | test fixtures, documentation |

Observed branch size:

- 155 changed files
- 9,921 insertions and 1,530 deletions
- 30 commits
- Initial feature commit alone: 106 files, 5,321 insertions, 840 deletions
- Latest CI: 1,754 `harness-server` tests passed and 12 failed

## High Findings

### H1: One scope guard became four system changes

- Evidence: commits `b5d6952c` through `03ac8755` modify merge authorization,
  leases, remote ownership, and GitHub mutation fencing after the classifier
  feature was already introduced in `aeec49a1`.
- Evidence: [the runtime V2 contract](workflow-runtime-v2-state-machine-spec.md)
  defines `ready_to_merge` completion as operator approval or external merge
  reconciliation; classifier delivery does not require redesigning merge
  execution.
- Fact: merge safety and classifier execution share changed files in PR #2008,
  but they do not share one acceptance criterion.
- Impact: every classifier review is forced to re-audit irreversible GitHub
  mutations and lease races. The review surface grows faster than the feature.
- Confidence: High.
- Suggested fix: keep all merge, lease, `server_owned`, and auto-merge changes
  out of the classifier recovery series. Track valid security findings
  independently.

### H2: The supposedly generic classifier owns GitHub-specific fact fetching

- Evidence: PR #2008 `crates/harness-server/src/workflow_runtime_worker/classifier.rs:142-245`
  fetches exact GitHub issue facts, validates PR URL identity, fetches the PR
  snapshot, and assembles the complete diff.
- Fact: the runtime driver knows `classify_change_scope`, `pr_scope_review`,
  GitHub repository identity, issue numbers, and PR URLs.
- Impact: adding another classifier requires editing the same server driver;
  the abstraction is a GitHub scope executor disguised as a generic activity.
- Confidence: High.
- Current recovery action: discard the PR #2008 classifier driver in favor of
  the `agent_contract` implementation now on `main`. For the PR scope guard,
  collect GitHub facts through an ordinary agent prompt, persist the validated
  snapshot with provenance, and hand that snapshot to the existing no-tool
  contract activity. Harness crates must not invoke GitHub or git directly.

### H3: Built-in policy pinning became a global admission dependency

- Evidence: the latest CI failures include repeated `MissingHash` failures in
  submission and PR-feedback paths that are not classifier execution tests.
- Evidence: commits `20f59db2`, `b3920287`, `41648eb0`, and `99c7fe38` repeatedly
  change server test bootstrap and fixtures to satisfy the new policy/pin
  requirements.
- Fact: adding the classifier made existing workflow construction and replay
  paths depend on new definition and classifier-policy identity fields.
- Impact: unrelated runtime tests and historical rows fail before reaching
  their original behavior. This is migration coupling, not a local scope gate.
- Confidence: High.
- Current recovery action: use the contract, prompt, input, provenance, and
  definition pinning already implemented by `agent_contract`. Do not add a
  classifier policy identity or require a definition hash on historical
  built-in rows.

### H4: Built-in workflow versioning was designed during implementation

- Evidence: commits `351ff3bc` through `a4372e14` successively change built-in
  hashes, historical definitions, selectors, terminal queries, retention, and
  service transitions.
- Fact: the first implementation changed the built-in state graph before a
  complete persisted-instance compatibility contract existed.
- Impact: each new consumer of definition identity exposed another replay or
  query mismatch. Review became the mechanism for discovering the migration.
- Confidence: High.
- Suggested fix: deliver multi-version built-in registry and selector semantics
  as a behavior-neutral prerequisite. Historical v1 rows resolve by their
  existing definition id and version, without a newly required content hash.
  Only then introduce `github_issue_pr@2`.

### H5: The branch is not a verified deliverable

- Evidence: PR #2008 CI run `33058376988`, job `98470884178`, failed with 12
  `harness-server` tests. Failures include stale state expectations, missing
  hashes, and a mismatched PR snapshot identity.
- Fact: the branch is open, not approved, and its required `CI Result` check is
  failing.
- Impact: continuing to patch this branch would repeat the same fixture and
  compatibility loop without reducing review scope.
- Confidence: High.
- Suggested fix: freeze PR #2008 and rebuild from `origin/main` in bounded,
  dependency-ordered PRs.

## Medium Findings

### M1: Test fixtures duplicate workflow identity construction

- Evidence: CI failures repeatedly report `MissingHash` or v1/v2 state
  mismatches in test-created instances.
- Fact: tests construct built-in workflow instances with literal versions,
  states, and partial data instead of using one production-equivalent builder.
- Impact: workflow contract changes create broad fixture churn and can hide
  whether production or the fixture is wrong.
- Confidence: High.
- Suggested fix: add version-specific fixture builders as part of the separate
  built-in versioning prerequisite, without changing production defaults.

### M2: Documentation describes the final combined system, not staged contracts

- Evidence: PR #2008 expands `docs/workflow-declarative-definitions.md` from the
  generic classifier schema through provider attestation, GitHub diff
  completeness, built-in policy snapshots, and remote-host rejection.
- Fact: the documentation does not separate generic runtime guarantees from
  GitHub integration guarantees.
- Impact: reviewers cannot tell which guarantees belong to which delivery
  boundary.
- Confidence: High.
- Current recovery action: treat the generic `agent_contract` path as delivered.
  Document only the remaining behavior-neutral built-in versioning and the
  prompt-driven GitHub fact snapshot plus v2 routing integration.

## Recovery Classification

| Classification | PR #2008 material | Recovery action |
|---|---|---|
| Superseded by `main` | `WorkflowClassifierPolicy`, verdict-route validation, provider-reported model event, server-authored assessment shape | Use `WorkflowAgentContract`; do not recreate or cherry-pick the classifier path |
| Rewrite | `workflow_runtime_worker/classifier.rs`, classifier prompt packet construction, assessment persistence | Keep GitHub collection in an agent prompt, persist one provenance-covered snapshot, and reuse the existing assessment path |
| Rewrite after prerequisite | `scope_review.rs`, `github_issue_pr` state changes, head-change rechecks, built-in policy resolution | Wait for behavior-neutral built-in version registry, then add one PR-scope gate |
| Independent follow-up | complete GitHub PR diff collector | Deliver with the GitHub integration or as its own fact-provider PR; it must not modify merge execution |
| Independent security work | `server_merge.rs`, `merge_completion.rs`, `job_claim.rs`, leases, server-owned routing, atomic base authorization | Move findings to separate issues/PRs with their own threat model |
| Discard from recovery | fixture-only compatibility commits and merge-behavior assertion rewrites | Recreate only tests required by each bounded contract |

## Current Decision (2026-09-05)

PR #2008 is historical evidence, not a merge candidate. Its generic classifier
was superseded by the delivered `WorkflowAgentContract` path. No commit from it
should be cherry-picked wholesale.

The prerequisite/versioning suggestions and classification table above are
historical audit recommendations, not an approved implementation plan. In
particular, a multi-version built-in registry and historical-row compatibility
are not prerequisites for the current reliability work. The remaining PR scope
integration is deferred and described in
[`model-scope-classifier-plan.md`](model-scope-classifier-plan.md).

Current work is limited to structured PR repair outcomes, live evaluation
(#1768), and cost observation/budget verification (#1770). Merge-lifecycle and
vNext proposals remain outside this queue.
