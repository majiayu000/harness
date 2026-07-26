# Product Spec

## Linked Issue

GH-1766

## User Problem

Workflow completion decisions rest on the agent's own structured result. The
server has evidence machinery (`TransitionRule::required_evidence`, evidence
classes, reducer contract checks) but the four built-in workflow definitions
run with empty evidence requirements, the quality gate's validation commands
are executed only by an LLM, a PR number is bound from an agent artifact with
no existence check, and a prompt task reaches `done` on a bare `succeeded`
status. Operators cannot distinguish a verified completion from a confident
claim, and every downstream signal (coverage accounting, dashboards, skill
quality, GC) inherits unverified state.

## Goals

- Make every fact-minting transition of the built-in workflows require
  server-verifiable evidence, enforced fail-closed.
- Re-execute quality-gate validation commands server-side and record an
  output digest as the gate's authoritative evidence.
- Verify PR existence and head ownership before `BindPr` is applied.
- Require validation evidence or an explicit no-change rationale before a
  prompt task completes.

## Non-Goals

- No new workflow states, no renamed states, and no changes to any state
  graph or transition allowlist topology.
- No changes to declarative YAML workflow semantics (they already enforce
  `required_evidence`).
- No removal or redesign of the agent-authored `harness-activity-result`
  block; it remains an input to decisions, never the verdict.
- No changes to reconciliation's authority over terminal states.
- No changes to the auto-merge gate, PR-feedback inspection, or zero-output
  detection (already server-owned).
- No redesign of `QualityGrader` beyond honest empty-window output.

## User-Visible Behavior

1. **B-001:** Every transition listed in the Evidence Contract below rejects
   a decision that does not carry the named evidence class. Rejection is a
   typed blocked outcome with a stable reason code; it is never downgraded to
   a warning or reported as success.
2. **B-002:** `required_evidence` enforcement applies uniformly to built-in
   and declarative definitions through one code path. Declarative workflows
   observe no behavioral change.
3. **B-003:** The quality gate passes only when the server itself has
   executed the configured validation commands in the workflow's workspace
   and recorded a validation digest (command, working directory, exit code,
   output hash, duration) as evidence. An agent-emitted `QualityPassed`
   signal without a matching server digest does not satisfy the gate.
4. **B-004:** A server validation run that fails, times out, or cannot start
   produces `QualityFailed` or a blocked outcome with the digest (or the
   startup error) attached. Absence of a configured validation command is an
   explicit, visible gate outcome — never a silent pass.
5. **B-005:** `BindPr` is applied only after the server resolves the claimed
   PR: it exists, is open, targets the expected repository, and its head is
   consistent with the workflow's branch. Verification produces
   `verified_pr_binding` evidence (PR number, head OID, observation time).
6. **B-006:** A `BindPr` claim that fails verification produces a typed
   blocked decision; the workflow does not enter `pr_open` and no coverage
   claim is recorded for the issue.
7. **B-007:** A prompt task reaches `done` only when its result carries a
   `validation_report` artifact (command list and exit codes) or an explicit
   structured `no_change_rationale`. When either is present, the server
   records a single `prompt_completion_evidence` evidence entry naming which
   alternative satisfied it, and the `implementing → done` transition
   requires that evidence kind. Otherwise the decision is blocked with
   reason `prompt_completion_evidence_missing`.
8. **B-008:** Prompt-task continuation semantics (external-state signals,
   attempt budgets, scope-too-large) are unchanged; only the terminal step
   gains the evidence requirement.
9. **B-009:** Reconciliation-driven terminal transitions (merged PR, closed
   issue) remain valid without agent evidence, as today.
10. **B-010:** Evidence rejection leaves the workflow instance unchanged
    except for the recorded blocked decision; no partial state or metadata
    mutation from the rejected decision persists.
11. **B-011:** An empty observation window grades as `Unknown` rather than
    100/A; consumers that key on Grade A see no change for non-empty
    windows.

## Evidence Contract

| Workflow | Transition | Required evidence class |
| --- | --- | --- |
| `github_issue_pr` | `implementing → pr_open` | `verified_pr_binding` |
| `github_issue_pr` | any non-reconciliation `→ done` | `github_pr` + `server_pr_snapshot` |
| `prompt_task` | `implementing → done` | `prompt_completion_evidence` (umbrella kind) |
| `quality_gate` | `checking → passed` | `server_validation_digest` |
| `pr_feedback` | `inspecting → ready_to_merge` | `server_pr_snapshot` |

All other transitions keep their current requirements. Evidence classes are
produced server-side; agent-authored artifacts may inform but cannot satisfy
them.

## Acceptance Criteria

- [ ] Table tests per built-in definition prove each contracted transition
      rejects a decision missing its evidence class and accepts one carrying
      it, with the typed reason code asserted.
- [ ] A declarative-workflow regression test proves unchanged behavior
      through the unified enforcement path.
- [ ] A quality-gate integration test proves an agent `QualityPassed` claim
      without a server digest blocks, and a server re-run failure records the
      digest and fails the gate.
- [ ] A missing-validation-command configuration produces a visible gate
      outcome, asserted by test.
- [ ] `BindPr` tests cover: nonexistent PR, closed PR, wrong repository,
      mismatched head — each yields a blocked decision and no `pr_open`
      transition; a valid PR binds with `verified_pr_binding` evidence.
- [ ] A prompt-task test proves prose + `succeeded` with no validation
      artifact blocks with `prompt_completion_evidence_missing`; tests cover
      both the `validation_report` and `no_change_rationale` paths to done.
- [ ] Reconciliation terminal-path regression tests pass unchanged.
- [ ] Rejected decisions leave the persisted instance row unchanged except
      for the blocked decision record, proven by snapshot comparison.
- [ ] Empty-window grading returns `Unknown`, asserted by test.

## Boundary Checklist

| Boundary | Verdict |
| --- | --- |
| Empty / missing input | Covered by B-001, B-004, B-007: missing evidence or missing validation configuration is a visible blocked/failed outcome, never a silent pass. |
| Error and failure paths | Covered by B-004, B-006; verification and validation failures are typed outcomes with evidence attached. |
| Authorization / permission | Verification uses the server's existing GitHub credentials; no new authority is granted to agents. |
| Concurrency / race / ordering | Evidence checks run inside the existing decision-transition transaction and row lock; B-010 guarantees rejected contenders persist nothing. |
| Retry / repetition / idempotency | A retried activity re-presents evidence; verification is re-run on each bind attempt; digests are per-execution and append-only. |
| Illegal state transitions | Covered by B-001/B-002; the transition allowlist topology is unchanged, only its evidence requirements tighten. |
| Compatibility / migration | Existing rows need no migration; in-flight workflows encounter the new requirements at their next transition (see Rollout Notes). |
| Degradation / fallback | Covered by B-001 and B-004: enforcement never downgrades to warnings; absence of data blocks. |
| Evidence and audit integrity | Server digests and `verified_pr_binding` are server-authored and hash-anchored; agent artifacts cannot forge them. |
| Cancellation / interruption / partial completion | A validation run interrupted mid-execution records a failed digest; the gate does not pass on partial output. |

## Edge Cases

- The agent claims `QualityPassed` while the server re-run fails on the same
  commands (divergent environments must surface, not average out).
- A PR exists but was opened by an unrelated actor against the same branch
  name in a fork.
- The claimed PR is merged or closed between the agent's claim and server
  verification.
- A prompt task legitimately requires no code change (documentation answer)
  and uses `no_change_rationale`.
- Validation commands are configured but the workspace was reclaimed before
  the server re-run starts.
- A reconciliation done-transition arrives for a workflow that is mid-flight
  under the new evidence rules.

## Rollout Notes

Enforcement lands behind a single runtime configuration flag defaulting to
enabled, with a documented kill switch for one release. In-flight workflows
meet the new requirements at their next transition; operators should expect a
one-time surfacing of previously invisible unverified completions as blocked
decisions with typed reason codes. Reverting restores trust-by-claim behavior
but not any corrupted state, since rejected decisions persist nothing.
