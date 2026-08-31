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

## Final production evidence

Final submission:

- submission id: `9e500996-3943-4bbc-b8b9-f75dcb438ea0`
- runtime job id: `7e003e96-d3c1-4995-8fa5-c9eb917192f1`
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

- submission to persisted artifacts: `14.556s`
- runtime job lifetime: `13.672s`
- agent-contract attempt: `12.977s`

Usage returned by the runtime submission API and persisted in
`runtime_usage_events`:

- input tokens: `21,757`
- output tokens: `140`
- reported total tokens: `21,897`
- provider-reported cost: `$0.00`

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

## Focused verification

The implementation is covered by:

```text
cargo test -p harness-server agent_contract_submission_command_matches_the_committed_instance
HARNESS_DATABASE_URL=<isolated-test-database> cargo test -p harness-server real_submission_assessment_routes_and_reopens_without_model_replay
```

The first test protects the submission/commit/dispatch fact identity. The
second protects assessment routing, durable reopen behavior, and exact token
and cost persistence for a contract attempt.
