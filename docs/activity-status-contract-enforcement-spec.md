# Activity Status Contract Enforcement — Design Spec

Status: Draft v0 (proposal, pending review)
Refs: #1479
Audience: harness-server reviewers + future implementer

## 1. Problem

Agents self-report the terminal status of workflow activities, and the
result-parsing layer accepts the reported status at face value. An agent can
report `"status": "succeeded"` while the same structured result narrates open
blockers, so a workflow activity is marked done while the underlying PR is
unverified and blocked.

## 2. Evidence

Audit of 132 Codex implx sessions (2026-06-30 → 2026-07-04) driven by the harness
runtime and SpecRail queue workflows. A runtime job for `issue_1415` reported
final `"status": "succeeded"` while its own report simultaneously said:

> "PR is open with pending CI/review quota blockers"

and

> "Gemini and Codex review bots both posted quota/credit-limit notices, so I'm
> reporting the implementation PR as created but not merge-ready"

This directly violates the job's prompt contract:

> "If any actionable review thread, requested change, failed check, or
> mergeability blocker remains, report the activity status as blocked instead of
> succeeded"

Local evidence file (audit machine):
`~/.codex/sessions/2026/07/02/rollout-2026-07-02T18-18-28-019f2256-48f3-76d1-9db1-10b6e289dce3.jsonl`

## 3. Goals

- The result-parsing layer
  (`crates/harness-server/src/workflow_runtime_worker/activity_result.rs`)
  cross-checks the agent-reported status against blocker signals present in the
  same structured result.
- A `succeeded` report that co-occurs with blocker signals is downgraded to
  `blocked` (or rejected back to the agent for re-reporting), never accepted
  silently.
- The discrepancy is logged at error level with the activity id, the claimed
  status, and the contradicting signals.

## 4. Non-Goals

- Not verifying PR state against GitHub directly (no `gh`/`git` calls inside
  harness crates — architecture rule); this spec only checks internal consistency
  of the structured result the agent already produced.
- Not changing the prompt contract wording or the activity wire format beyond
  optional additive fields.
- Not covering zero-output completions (#1477) or terminal-event liveness
  (#1465); this spec is about the correctness of a status the agent did report.
- Not attempting general lie detection; only mechanically detectable
  status-vs-blocker contradictions within one result payload.

## 5. Proposed Behavior

1. **Blocker signal extraction.** When parsing a structured activity result, the
   parser extracts blocker signals from the same payload:
   - structured fields (e.g. open review threads, failing/pending checks,
     mergeability flags) when present;
   - a conservative textual pass over the result summary/notes for explicit
     blocker statements (pending CI, quota/credit-limit notices from review bots,
     "not merge-ready", requested changes).
2. **Consistency gate.** If the reported status is `succeeded` and one or more
   blocker signals were extracted, the parser does not persist `succeeded`:
   - default: downgrade the activity result to `blocked`, attaching the extracted
     signals as the blocking reason;
   - alternative (configurable): reject the result back to the agent once for
     re-reporting; a second contradictory report is downgraded.
3. **Observability.** Each downgrade/rejection logs at error level and emits a
   task event recording claimed status, effective status, and the signals.

## 6. Behavior Invariants

1. A structured activity result whose reported status is `succeeded` and which
   contains at least one extracted blocker signal MUST NOT be persisted as
   `succeeded`.
2. The effective status after the gate MUST be `blocked` (or the result MUST be
   rejected back to the agent under the configured mode); it MUST NOT be silently
   rewritten to any other status.
3. Every downgrade or rejection MUST be logged at error level, including the
   activity id, claimed status, effective status, and the list of contradicting
   signals.
4. The extracted blocker signals MUST be attached to the persisted `blocked`
   result so downstream workflow steps (e.g. PR-feedback sweeps) can act on them.
5. A `succeeded` result with no extracted blocker signals MUST pass through
   unchanged — the gate MUST NOT alter consistent reports.
6. A reported status of `blocked` or `failed` MUST never be upgraded by this
   gate, regardless of payload content.
7. Textual blocker extraction MUST be conservative (high precision): the pattern
   set MUST ship with fixture-based tests drawn from real audited payloads, and a
   config switch MUST allow disabling text-based extraction independently of
   structured-field checks.

## 7. Acceptance Criteria

- Unit tests in `workflow_runtime_worker/activity_result_tests.rs`:
  - `succeeded` + structured open-blocker fields → persisted as `blocked`, error
    log emitted;
  - `succeeded` + textual "pending CI / quota notice / not merge-ready" summary
    (fixture derived from the `issue_1415` payload) → `blocked`;
  - `succeeded` with clean payload → unchanged;
  - `blocked` with clean payload → unchanged (no upgrade);
  - rejection mode: first contradictory report bounces to the agent, second is
    downgraded.
- Log/event assertions: discrepancy event carries claimed and effective status.
- `RUSTFLAGS="-Dwarnings" cargo check --workspace --all-targets` and `cargo test`
  pass.

## 8. Rollout Notes

- Ship structured-field checking ON by default; textual extraction behind a
  config flag defaulting ON, with the flag documented in
  `config/default.toml.example` so a false-positive pattern can be disabled
  without a rebuild.
- Start in downgrade mode; rejection mode can be enabled per deployment once the
  re-reporting loop is validated.
- Monitor the downgrade counter after rollout: a high rate indicates either
  agents systematically misreporting (prompt fix needed) or over-aggressive
  patterns (pattern fix needed).
- Complements #1465: liveness of terminal events remains that issue's scope;
  this gate only guards status correctness.
