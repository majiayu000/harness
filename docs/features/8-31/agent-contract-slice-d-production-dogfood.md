# Agent Contract Slice D: Production Dogfood

Date: 2026-08-31

Last verified: 2026-09-01

## Scope

Slice D exercises the merged Slice C contract path through a real production
server dispatch. The run is intentionally bounded to Codex, one declarative
activity, one pinned semantic verdict schema, no tools, no mutation, and an
ephemeral empty workspace. It does not enable automatic merge, the Cutover
RFC, or any vNext phase.

The production profile used:

- runtime kind: `codex_exec`
- runtime profile: `slice-d-sanitized-codex`
- model: `gpt-5.6-sol`
- reasoning effort: `high`
- approval policy: `never`
- primary attempts: `1`
- correction attempts available: `1`
- runtime timeout: `300s`
- USD budget enforcement: `shadow`

The server ran on an isolated local port with an isolated disposable
PostgreSQL database and an isolated XDG configuration root. The submission was
created through `POST /api/workflows/runtime/submissions`.

## Credential isolation

Harness recognizes `ANTHROPIC_API_KEY` as the Claude backend credential. The
Codex-safe server launcher now always removes that variable before starting the
server, along with inherited Codex and Claude wrapper-session variables. Its
integration test injects a sentinel Anthropic key into the parent environment
and proves the launched process cannot read it.

The final production run injected a sentinel `ANTHROPIC_API_KEY` into the
launcher environment. The launch record contains `-u ANTHROPIC_API_KEY`, and a
direct check of the live server process environment confirmed the variable was
absent before submission. The selected and observed runtime was Codex; no
Claude backend or credential participated in the run.

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

The final fresh review also identified that Codex reports token counts but not
provider USD cost. Harness does not invent a price. A backend must now
explicitly claim that its streamed usage contains reported USD cost before an
agent contract can launch under `enforce`; otherwise the activity blocks before
model execution. Shadow and unlimited policies remain available. Mid-turn
ceiling exhaustion from a cost-reporting backend is retained as a structured
server-authored budget stop. The completion transaction recognizes that marker
before declarative `on_blocked` routing, so it cannot be redirected into a
terminal state instead of the operator-only budget block.

Integration review against the latest `main` closed two final cancellation and
accounting races. Timeout or lease-loss cleanup now propagates a budget stop
found while draining already-queued usage, and `enforce` requires each launched
attempt to emit an observed usage event before its verdict can be accepted. A
provider-reported zero cost remains valid; an absent usage event does not. The
first drained budget stop remains authoritative even if a later queued usage
event is malformed.

## Final production evidence

Final submission:

- submission id: `1315b97d-1369-4a67-83c1-f4412e33d12c`
- runtime job id: `0ebdc15c-ffb8-4687-a32c-87839aafb2c2`
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

- submission to persisted artifacts: `24.545s`
- runtime job lifetime: `23.756s`
- agent-contract attempt: `22.745s`

Usage returned by the runtime submission API and persisted in
`runtime_usage_events`:

- input tokens: `21,807`
- output tokens: `222`
- reported total tokens: `22,029`
- provider-reported cost: `$0.00`

The persisted usage row is attributed to project
`/private/tmp/harness-slice-d-sanitized-20260901/project` and task
`1315b97d-1369-4a67-83c1-f4412e33d12c`. Its stable turn id is
`agent-contract:0ebdc15c-ffb8-4687-a32c-87839aafb2c2:1`.

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

- submission response: `f092acb874a3ee750ad9b38ecae4f1120d865a14d4743d8f12772a57bd42f65f`
- artifacts response: `62f6f50120d197795f277ecd0705dbf441808bc2ded490ad3496381a36bca112`

## Focused verification

The implementation is covered by:

```text
scripts/test-binary-freshness.sh
cargo test -p harness-server agent_contract_submission_command_matches_the_committed_instance
HARNESS_DATABASE_URL=<isolated-test-database> cargo test -p harness-server real_submission_assessment_routes_and_reopens_without_model_replay
HARNESS_DATABASE_URL=<isolated-test-database> cargo test -p harness-server usage_survives_backend_failure_after_report
HARNESS_DATABASE_URL=<isolated-test-database> cargo test -p harness-server enforced_budget_rejects_terminal_verdict_after_usage_crosses_ceiling
HARNESS_DATABASE_URL=<isolated-test-database> cargo test -p harness-server lease_loss_drains_queued_usage_before_returning
HARNESS_DATABASE_URL=<isolated-test-database> cargo test -p harness-server lease_loss_preserves_budget_stop_before_later_usage_error
HARNESS_DATABASE_URL=<isolated-test-database> cargo test -p harness-server enforced_budget_requires_backend_reported_cost
HARNESS_DATABASE_URL=<isolated-test-database> cargo test -p harness-server enforced_budget_requires_an_observed_usage_event
HARNESS_DATABASE_URL=<isolated-test-database> cargo test -p harness-workflow budget_ceiling_preempts_terminal_agent_contract_completion
HARNESS_DATABASE_URL=<isolated-test-database> cargo test -p harness-workflow budget_stop_preempts_declarative_on_blocked_terminal_route
```

The first test protects the submission/commit/dispatch fact identity. The
second protects assessment routing, durable reopen behavior, exact token and
cost persistence, and project/task attribution. The remaining tests protect
failed-attempt accounting and observations, enforced mid-attempt and
transaction-time budget stops, and queued usage during lease cancellation. The
production success reported zero cost, so the transaction-time ceiling is
proved by the focused database test rather than that successful run. Codex
cannot silently enter that enforced path: its missing cost-reporting capability
is covered by the preflight test and blocks before launch.
