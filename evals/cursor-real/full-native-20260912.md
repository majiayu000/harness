# Full native Cursor development evaluation — 2026-09-12

**Status: this 15-case development run is complete. All 15 native workflows reached ready_to_merge and all independent assessments returned. This is not full acceptance of all 15 cases or merge authorization.**

This run routes eight Issue cases and seven PR cases through the native Harness workflow runtime using real Cursor CLI processes. It is an exploratory development run, not a calibrated benchmark or held-out comparison. No evaluation PR is merged.

## Reproduction and evidence

- Evidence root: `/Users/apple/.local/share/harness/cursor-evals/20260912-full-native`.
- Isolated PostgreSQL database: `cursor_full_native_20260912`; server port `19840`. Credentials are excluded from this report.
- Four simultaneous real Cursor processes; requested model `auto`, underlying model and complete token/cost accounting unknown.
- All 15 dedicated private evaluation repositories start from frozen source revisions with full ancestry. Later upstream solutions are withheld by instruction. This is not a proven contamination-free setup.
- `prepared.json`, `submissions.json`, archived operator WORKFLOW files, full job/event/artifact exports, per-case grader inputs/results and binary/source checksums are retained locally.
- Cursor CLI: `2026.09.10-fd3934a`. The batch changed binary after five planning failures, with zero running jobs at transition; see `evidence/variant-transition.json` and `evidence/build-source-plan-fix/`. Do not treat all attempts as a fixed-version trial.

## Case inventory

| Case | Frozen start | Evaluation repository |
|---|---|---|
| issue-lifecycle | `af5bcd26033b550cb4d0b6ddb16ff959df65a7aa` | majiayu000/hce-0912-issue-lifecycle |
| startup-recovery | `f340f03316cab2a1930daec4b72f02a47b3007e2` | majiayu000/hce-0912-startup-recovery |
| context-protocol | `e68fdeb6ed25292cd91ff55f60d6c51ef3986263` | majiayu000/hce-0912-context-protocol |
| intake-coverage | `505961e3c38bd575650e876baec29c820ec00a7e` | majiayu000/hce-0912-intake-coverage |
| durable-transcript | `40ab28c9abff26d78901c3c3d614c49fca0cfee7` | majiayu000/hce-0912-durable-transcript |
| bun-toolchain | `856bb080e72e064ccc88360a3b1921bf0e8583b1` | majiayu000/hce-0912-bun-toolchain |
| definition-visibility | `4888b404f205927925064c6852d165e6f41c2794` | majiayu000/hce-0912-definition-visibility |
| intake-routing | `50116e93ce5bdbdc2cea28967b614b4f43d21eac` | majiayu000/hce-0912-intake-routing |
| techpulse-pr | `0c369035215243ea4528d3ff75b4a5e74a70e2a9` | majiayu000/hce-0912-techpulse-pr |
| loom-pr | `2abc521f947cfdb059dbe28a42a5347829391e55` | majiayu000/hce-0912-loom-pr |
| keepline-pr | `a2bd28daabb5475ff083c03829709410b2970d9d` | majiayu000/hce-0912-keepline-pr |
| ccstats-pr | `515ec99c67440839e0731c8b13f7f71f80406bd9` | majiayu000/hce-0912-ccstats-pr |
| operator-snapshot-pr | `ba2a51f825815bce41b91eb6c6101c754f27bae7` | majiayu000/hce-0912-operator-snapshot-pr |
| vibeguard-pr | `9f658f0854b9af5f04d13c54bec6c7a69f5d3c1f` | majiayu000/hce-0912-vibeguard-pr |
| rekey-pr | `6b7abf16d28fc40242c2c4052046ffb2ca787e17` | majiayu000/hce-0912-rekey-pr |

## Final results

Run window: 2026-09-12 20:21 to 2026-09-13 01:55 Asia/Shanghai (about 5 h 34 min). All 15 evaluation PRs remain OPEN; none was merged.

- 103 genuine Cursor activities: 97 completed successfully, 6 execution failures. Successful assessment execution is not candidate acceptance.
- 13 planning activities (including five failed first attempts), 8 Issue implementations, 20 feedback repairs, 40 local reviews and 22 independent evaluator activities.
- 103 retained Cursor transcripts contain 5,494 `tool_call` items. These are recorded stream items, not a deduplicated shell-command count.
- All 103 token-usage objects contain zero input/output/total tokens and zero cost. These fields are unavailable telemetry, **not evidence of free execution or zero usage**. Actual billed token/cost totals cannot be inferred from this run.
- Latest grader labels: 14 ACCEPTED, 1 INCOMPLETE. Several ACCEPTED labels retain material coverage limitations below; do not report 14/15 as a strict autonomous end-to-end success rate.

| Case | Exact final head (short) | Latest grader | Evidence and remaining limits |
|---|---|---|---|
| [issue-lifecycle](https://github.com/majiayu000/hce-0912-issue-lifecycle/pull/2) | `f67a89049f46` | ACCEPTED | Disposable PostgreSQL transition/rollback tests, 224 transition pairs; main CI skipped by evaluation base. |
| [startup-recovery](https://github.com/majiayu000/hce-0912-startup-recovery/pull/2) | `193eb6deb574` | ACCEPTED | CAS classifications, fail-closed conflicts and applied-only counting verified on disposable PostgreSQL; no independent execution-process crash/concurrent-writer live experiment established. |
| [context-protocol](https://github.com/majiayu000/hce-0912-context-protocol/pull/2) | `14e0434e37cd` | ACCEPTED | Dependency boundary, conversion tests and live context/preview RPC verified; main CI skipped. |
| [intake-coverage](https://github.com/majiayu000/hce-0912-intake-coverage/pull/2) | `e8c903dbbe7e` | ACCEPTED | Real empty-runtime GitHub intake, open closing PR recovery, zero duplicate implementation and restart idempotency verified; assisted repair after first grader rejection. |
| [durable-transcript](https://github.com/majiayu000/hce-0912-durable-transcript/pull/2) | `db80538a7f87` | ACCEPTED | Real Cursor bytes reconstructed into candidate storage, actual server restart and HTTP read verified. Live producer attachment across a turn was not independently re-proven; historical candidate has no Cursor backend. |
| [bun-toolchain](https://github.com/majiayu000/hce-0912-bun-toolchain/pull/2) | `44515bb17513` | ACCEPTED | Pinned Bun, local web checks and real Web Build → Rust Test verified. CI Result remains FAILED: Clippy and Security Audit. Temporary fixture permissions restored to read. |
| [definition-visibility](https://github.com/majiayu000/hce-0912-definition-visibility/pull/2) | `cb1387fb5975` | ACCEPTED | Real active/failed/done declarative endpoints and restart verified with genuine Codex inside historical candidate; outer author/review/evaluator are Cursor. Nested Cursor integration unavailable. |
| [intake-routing](https://github.com/majiayu000/hce-0912-intake-routing/pull/4) | `fd97f3c7c2c9` | ACCEPTED | Real GitHub route, subject/trust metadata and live dedupe verified. Unmatched fixture was mislabeled; cap/multi-match/terminal-reopen primarily fixture tests. No simulated Agent output counted as real evidence. |
| [techpulse-pr](https://github.com/majiayu000/hce-0912-techpulse-pr/pull/1) | `b5b83c96d29c` | ACCEPTED | Bounded/SSRF-safe collectors and validation checked after supervisor caught a missed RSS/HN/Reddit gap; assisted acceptance. No hosted CI. |
| [loom-pr](https://github.com/majiayu000/hce-0912-loom-pr/pull/1) | `99b5c351ebf2` | ACCEPTED | Starting coverage mismatch reproduced; Vitest 5 + coverage 5, typecheck, 193 tests and coverage passed. |
| [keepline-pr](https://github.com/majiayu000/hce-0912-keepline-pr/pull/1) | `a2bd28daabb5` | INCOMPLETE | Genuine provider live ownership restoration remains unverified. Initial renamed-child provider evidence rejected; generic process checks do not satisfy it. |
| [ccstats-pr](https://github.com/majiayu000/hce-0912-ccstats-pr/pull/1) | `cd0ba413d28d` | ACCEPTED | Actual Windows Desktop/CLI CI plus Darwin checks passed after five repairs and one stalled-review retry. |
| [operator-snapshot-pr](https://github.com/majiayu000/hce-0912-operator-snapshot-pr/pull/1) | `53d34a8c5913` | ACCEPTED | Genuine Cursor success/failure rows and candidate HTTP snapshot verified. Provider runs used platform binary; no live aged Cursor stall; child-definition recent-failure enumeration remains a residual concern. |
| [vibeguard-pr](https://github.com/majiayu000/hce-0912-vibeguard-pr/pull/1) | `8009b63537c2` | ACCEPTED | Disposable installation, manual hook behavior, size limits and third-party preservation checked; no hosted CI. |
| [rekey-pr](https://github.com/majiayu000/hce-0912-rekey-pr/pull/1) | `9172eaa67db7` | ACCEPTED | Real Vault 1.20.3 TLS and PostgreSQL on Ubuntu CI plus independent Linux Docker verification; Layer A scope only. |

The local `evidence/final-results.json` records each exact full SHA, final PR state, grader job ID, verbatim latest grader summary, per-case stage counts and failed job IDs. Earlier failed/rejected/incomplete assessments remain in the exported jobs and transcripts.

### What remains before stronger claims

- Complete genuine Keepline provider ownership recovery and the specific live-production-path gaps listed above. Grader labels must not waive missing acceptance evidence.
- Harden Harness operator-config isolation and per-command dispatcher error containment; make nested error causes visible. The restored evaluation fixture fixed this run's blockage, not those product weaknesses.
- Distinguish absent Cursor usage telemetry from measured zero values in reporting.
- Run unused, calibrated cases with a fixed Harness version and controlled repetitions before claiming prompt/memory improvement. This run changed prompt/configuration during execution and includes supervisor intervention.
- Check fresh independent review and actual required CI before any merge; this evaluation merged nothing and does not validate every merge boundary.

## Observed failures and interventions

1. Five real planning turns returned success with provisionable prerequisites or explicit “none” in blockers. The status contract rejected them. The planning prompt now requires an empty array when execution can proceed, while real unavailable access remains blocked. Focused prompt packet tests passed (57); all five genuine replans subsequently entered implementation. Unblock operations are retained, not hidden.
2. One CCStats review timed out after 600 seconds without output while waiting for Windows Desktop CI. Run `34701727651` later passed Windows Desktop and other checks; the native failed-workflow retry endpoint resumed independent review. The timeout remains a failed attempt.
3. The operator-snapshot evaluation WORKFLOW was replaced with the repository default, introducing Codex-only `approval_policy: never` for Cursor and production-oriented settings. A profile selection error aborted dispatcher ticks and delayed unrelated commands. Restoring the archived evaluation WORKFLOW immediately dispatched three queued commands. No product dispatcher fix has been applied in this batch.
4. TechPulse initial independent approval missed RSS/HN body bounds and the Reddit HTTP client. Supervisor source inspection fed these concrete findings back through native PR submission. Cursor reviewed, repaired and independently re-reviewed; a fresh grader accepted head `b5b83c96d29c1cb6dbbc6d4cf079272bbd36161c`. This is an assisted result, not first-pass autonomous success.
5. A Keepline grader used a normal child named Claude. That evidence was rejected as provider impersonation. A fresh evaluator reported INCOMPLETE because genuine provider ownership restoration was not exercised. Do not count either the impersonation or workflow readiness as acceptance.
6. Main-only GitHub CI was skipped for eval-base PRs. Bun was retargeted to a main alias at the identical frozen base and reopened without changing the candidate SHA. Real CI then failed in Detect Changes with `Resource not accessible by integration`; downstream Rust jobs initially did not run. After temporary evaluation-only Actions permission adjustment, actual Web Build and Rust Test passed while Clippy and Security Audit failed. The repository permission was restored to read. Both CI attempts are retained.
7. Read-only independent evaluation found missing real-server evidence for definition visibility and empty-runtime intake coverage. Both were resubmitted as native PR work; genuine reviews requested repair.
8. The operator clarified that existing authorized Cursor CLI login may be used for new genuine evaluation sessions. Credentials must not be read/copied/changed and unrelated sessions must not be operated. The earlier disposable-login interpretation caused avoidable missing-evidence outcomes. Harness databases/config/ports remain isolated.

9. Historical definition-visibility and durable-transcript candidate revisions have no Cursor backend. The outer implementation/review remains genuine Cursor. A genuine supported Codex backend may establish provider-independent historical behavior, but cannot establish nested Cursor integration. No unrelated adapter is added to make the benchmark pass.

## Interpretation

Native `ready_to_merge` is a workflow result, not the independent evaluator verdict. All activity failures, repair loops, stale/invalid verdicts, operator interventions and missing environments remain visible in the final case table. Pending CI, skipped CI, unavailable authentication and fixture tests do not prove real end-to-end acceptance.

This suite is Harness/Rust-heavy and does not establish complete WebGoat, Remem, frontend, merge-boundary or execution-crash coverage. Separate unused cases, fixed-version paired runs and repetitions are still required to measure generalization and stability.
