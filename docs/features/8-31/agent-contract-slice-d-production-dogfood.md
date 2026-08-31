# Agent Contract Slice D: Production Dogfood

Date: 2026-08-31

## Scope

Slice D exercises the merged Slice C contract path through a real production
server dispatch. The run is intentionally bounded to Codex, one declarative
activity, one pinned semantic verdict schema, no tools, no mutation, and an
ephemeral empty workspace. It does not enable automatic merge, the Cutover
RFC, or any vNext phase.

The production profile used:

- runtime kind: `codex_exec`
- runtime profile: `slice-d-codex-contract`
- model: `gpt-5.6-sol`
- reasoning effort: `high`
- approval policy: `never`
- primary attempts: `1`
- correction attempts available: `1`
- runtime timeout: `300s`

The server ran on an isolated local port with an isolated disposable
PostgreSQL database and an isolated XDG configuration root. The submission was
created through `POST /api/workflows/runtime/submissions`.

## Production findings

The first real submission exposed a dispatch-order bug. The command pinned the
initial semantic facts, then the atomic submission commit added the server-owned
`last_decision` and `execution_path` fields to the workflow instance. Dispatch
reconstructed the expected command from that committed instance and rejected
the otherwise valid command as a mismatch before model execution.

The fix writes those two already-required lifecycle fields when constructing
the declarative submission instance. The command and committed instance now pin
the same facts without weakening exact command validation. A focused regression
test reproduces the real commit sequence.

The next successful run exposed an observability gap: contract attempts read
Codex token events for security observation but did not persist them through
the existing runtime usage store. The fix retains the latest token event in the
attempt observations and writes it through the same `RuntimeUsageContext` used
by ordinary turns. The usage key is a stable per-job, per-attempt turn id, so
replay remains idempotent.

Fresh review then found three accounting blockers in that first implementation.
The final implementation persists each token event before handling a subsequent
backend failure, evaluates the existing workflow budget ceiling immediately
after persistence, and resolves the workflow before constructing the usage
context so project and task attribution come from the durable instance. Focused
tests cover failure-after-usage, a terminal verdict crossing an enforced
ceiling, and project/task attribution through the full submission path.

A second fresh review found three remaining failure-path gaps. The final repair
keeps the transaction-time budget fence authoritative for successful contract
completions, retains security observations from failed attempts, and stops then
drains the agent stream on timeout, lease loss, budget exhaustion, or accounting
failure. This prevents an already-queued usage event from being lost during
cancellation without adding a second accounting path.

## Final production evidence

Final submission:

- submission id: `fa8edffc-d198-40e9-86b3-53b06723b47c`
- runtime job id: `64854d15-c448-4a77-8843-a3eae43ec78e`
- terminal projection: `done:done`
- verdict outcome: `approved`
- observed model: `gpt-5.6-sol` (`launch_derived`)
- primary attempts used: `1`
- corrections used: `0`
- approval requests: `0`
- tool surface items: `0`
- tool output deltas: `0`
- unknown item kinds: `0`

Timing from server-authored PostgreSQL timestamps:

- submission to persisted artifacts: `16.745s`
- runtime job lifetime: `15.684s`
- agent-contract attempt: `13.540s`

Usage returned by the runtime submission API and persisted in
`runtime_usage_events`:

- input tokens: `21,789`
- output tokens: `132`
- reported total tokens: `21,921`
- provider-reported cost: `$0.00`

The persisted usage row is attributed to project
`/private/tmp/harness-slice-d-20260831/project` and task
`fa8edffc-d198-40e9-86b3-53b06723b47c`. Its stable turn id is
`agent-contract:64854d15-c448-4a77-8843-a3eae43ec78e:1`.

Harness does not synthesize a price when the Codex stream reports zero cost;
the stored and returned value remains the provider-reported value.

## Restart and replay

After the final terminal result, the server was stopped and restarted against
the same database and project declaration. The normalized submission response
and all three artifacts were identical before and after restart:

- `server_runtime_turn_observations`
- `agent_contract_verdict`
- `agent_contract_assessment`

The durable row counts also remained unchanged:

- runtime events: `5`
- usage rows: `1`
- completed contract attempts: `1`

No model attempt was replayed and no usage row was duplicated.

The canonical compact JSON hashes matched across the restart:

- submission response: `92b0f4ca0eb7d1c0f55d789ccc5339010d9302237b3507b12d48f8673d89957f`
- artifacts response: `fd102abd5fc4f01200293384149f9f789c6c308ecdf4f6b6327d8ca7e146430f`

## Focused verification

The implementation is covered by:

```text
cargo test -p harness-server agent_contract_submission_command_matches_the_committed_instance
HARNESS_DATABASE_URL=<isolated-test-database> cargo test -p harness-server real_submission_assessment_routes_and_reopens_without_model_replay
HARNESS_DATABASE_URL=<isolated-test-database> cargo test -p harness-server usage_survives_backend_failure_after_report
HARNESS_DATABASE_URL=<isolated-test-database> cargo test -p harness-server enforced_budget_rejects_terminal_verdict_after_usage_crosses_ceiling
HARNESS_DATABASE_URL=<isolated-test-database> cargo test -p harness-server lease_loss_drains_queued_usage_before_returning
HARNESS_DATABASE_URL=<isolated-test-database> cargo test -p harness-workflow budget_ceiling_preempts_terminal_agent_contract_completion
```

The first test protects the submission/commit/dispatch fact identity. The
second protects assessment routing, durable reopen behavior, exact token and
cost persistence, and project/task attribution. The remaining tests protect
failed-attempt accounting and observations, enforced mid-attempt and
transaction-time budget stops, and queued usage during lease cancellation. The
production success reported zero cost, so the transaction-time ceiling is
proved by the focused database test rather than that successful run.
