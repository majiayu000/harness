# Baseline regression verification

Baseline: `67eb412b`, following checkpoint `29b866e6`.

## Findings and changes

The previously failing parent recovery test supplied a successful PR inspection
with actionable feedback. Its expected blocked state depended on the removed
blocker-count convergence rule. Recovery identifiers identify the command to
resume if blocked; they do not themselves require blocking.

The test now verifies parent rejection of a child result missing required merge
readiness evidence. It retains the blocked-state and parent recovery command
identity assertions. A separate positive case verifies that actionable feedback
with those same recovery identifiers still requests repair. This reducer test
does not claim that every invalid child result is propagated by the worker.
Worker integration tests separately verify propagation and deliberate suppression.

Two database recovery tests still expected stale `blocked_reason`,
`failure_reason`, `unblock_hint`, and `retry_hint` fields to survive recovery.
Commit `42d1bef1` already clears these fields. The tests now require their removal
while preserving assertions for ordinary source data and historical error fields.
Recovery audits, dispatch commands, and state assertions remain checked.

No production Rust code, workflow configuration, deployment, or runtime
persistence behavior was changed in this follow-up.

## Verification

Commands used `CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=1`. Database-dependent tests
used a newly created disposable PostgreSQL 17 container, with its own database
and a loopback-only dynamically assigned port. No existing database was used.

All commands below use `cargo test -p <package> <filter> --lib`:

| Package | Filter | Passed |
| --- | --- | ---: |
| harness-workflow | pr_repair_evidence | 36 |
| harness-workflow | local_review | 33 |
| harness-workflow | runtime::store::recovery::tests | 6 |
| harness-workflow | runtime_recovery_ | 8 |
| harness-workflow | declarative_recovery_is_atomic | 1 |
| harness-workflow | runtime_worker_propagates_ | 4 |
| harness-workflow | runtime_worker_does_not_propagate_ | 2 |
| harness-workflow | runtime_worker_blocks_invalid_pr_feedback_child | 1 |
| harness-workflow | runtime::tests::retry:: | 18 |
| harness-server | runtime_task_route_tests | 20 |
| harness-server | runtime_dispatch_tests | 27 |

The recovery lock test normally ignored by `runtime_recovery_` was explicitly
run with filter `runtime_recovery_locks_instance_before_waiting_on_command` and
`-- --ignored`: one passed against the isolated database.

Initial filters based on source filenames matched zero tests; these were not
counted as coverage and were replaced with actual registered test names.

## Limits and next gate

These are deterministic regression and PostgreSQL integration results. They do
not measure live agent review quality, repair convergence, token consumption,
or production reliability. A read-only, isolated live workflow trial remains the
next gate before enabling a selected workflow in an operational project. No
live agent trial or production rollout is claimed here.

## Integration follow-up (2026-09-15)

The checkpoint was merged with main while retaining runtime operator snapshots and optional legacy TaskStore access. Fresh verification passed the server all-target check and 21 operator-snapshot tests against disposable PostgreSQL.

Independent source review identified two merge-review target lifecycle defects. Both were reproduced with failing regressions before correction:

- A readiness request wrote the current head before persistence cleared stale ready-state metadata, deleting its own target. Persistence now installs the newly requested target after invalidation; explicit resubmission continues to clear old approval.
- A changes-requested review retained the pre-repair target. Entering local repair now invalidates that target so the repaired commit can be reviewed before the server selects a fresh merge target.

Fresh validation passed 33 local-review tests, 27 server PR-feedback tests, 36 repair-evidence tests, and 8 runtime-recovery tests. One recovery concurrency test remained explicitly ignored in that filtered run. PostgreSQL tests used an isolated disposable database. The readiness regression covers request persistence, the real completion reducer, and persisted ready state; it is not a live GitHub merge trial.

A stale server test previously attempted a local pass directly from pr_open through a warning-only test wrapper and expected the former remote-review/quality-gate sequence. It now requests review first, propagates persistence errors, asserts ready_to_merge, and verifies late remote feedback creates no additional command. The unused wrapper was removed.

Fresh independent review approved the fixes, integration, and test adjustments without remaining blockers in the inspected paths. The large checkpoint was reviewed by risk area, not exhaustively line by line. CI and a bounded production trial remain separate evidence; no production service was restarted or deployed during integration.

## CI follow-up

The first full CI run exposed additional stale fixtures not exercised by the local DB-less push gate. JSONRPC worker fixtures registered only the oneshot backend, and three mocks lacked start_turn. They now register the turn factory and use the same in-memory execute_stream bridge as RuntimeStreamAgent. The model-facing prompt test checks the transition-owner instruction while retaining durable schema assertions. The probe-cap test accounts for a scheduled readiness review and still requires exactly one probe. Repair-result tests distinguish deferred remote readiness from explicit failing checks; failing checks and DIRTY merge state remain blocking.

Fresh isolated PostgreSQL reruns passed 2 continuation tests, 11 worker tests, the runtime-profile timeout test, the probe-cap test, and 7 repair-contract tests. The initial CI run also had poisoned environment-lock cascades after the root assertions failed; those failures are not claimed to be independent product defects.

CI identified RUSTSEC-2026-0285 in rustls 0.23.37. The lockfile now uses rustls 0.23.45 and its required rustls-webpki 0.103.15 patch. Under the unchanged audit configuration, cargo audit reports zero blocking vulnerabilities; pre-existing informational warnings remain. Four Markdown trailing-space violations were removed. Full CI must pass on the updated heads before merge.

## REST response ownership migration

The six remaining legacy handler inventory violations were resolved using the existing protocol-owned transparent response envelope pattern. Dashboard, overview, token usage, health, project queue stats, and intake status now return ContractJson with endpoint-specific harness-protocol response types. JSON construction, status codes, and token-usage error/empty responses remain unchanged. This establishes ownership without claiming field-level schema validation; no legacy inventory fixture or enforcement was changed.

Fresh validation passed cargo check --workspace --all-targets and all five legacy_rest_inventory tests. Filtered server tests passed for dashboard (12), overview (14), token_usage (10), health routes (15), queue stats (1), and intake/auth/list routes (18); the token route test overlaps two filters. Database-dependent cases used disposable PostgreSQL. Independent source review returned APPROVED for the response migration and wire compatibility.
