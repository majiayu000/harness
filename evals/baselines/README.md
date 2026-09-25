# Eval Baselines

Reviewed live-run baselines are stored under `evals/baselines/<suite>/` as
`latest.json` plus dated history. Do not create a baseline from dry-run,
collect-only, dispatch-failed, timed-out, or otherwise incomplete evidence.

The scheduled workflow starts in `report-only` mode. After a trusted full run
finishes on the isolated eval runner, add its report through normal review and
set `HARNESS_EVAL_GATE_MODE=enforce`. Enforced runs fail preflight when
`evals/baselines/harness-core/latest.json` is absent.

The live workflow also requires `HARNESS_EVAL_ENABLED=true`, the
`eval-nightly` environment, an isolated `self-hosted` runner labeled
`harness-eval`, a configured `HARNESS_DATABASE_URL`, an active Harness server,
and an online active runtime host advertising `runtime_job_lease_proof_v1`,
`eval_resource_limits`, `eval_network_policy`, and `trusted_eval_verifier_v1`.
The lease-proof capability is required to claim a job; the network-policy
capability is required to enforce the policy returned with an eval lease.
Trusted eval verifiers execute as native,
evaluator-owned code from the versioned declarative contract embedded in the
Harness binary. Advertising the verifier capability asserts that the runtime
host has the matching Harness revision. Keeping the workflow disabled before
those prerequisites exist prevents infrastructure absence from being
misreported as a benchmark regression.

If a report records `event_persistence_failed`, repair the observe stream with
`harness eval retry-events <report.json>`. The command re-emits deterministic
event IDs and atomically clears only the event-persistence outcome after the
write succeeds. Incomplete reports are rejected by `eval diff` and baseline
refresh eligibility checks.

After a green report-only run, manually dispatch the workflow with
`refresh_baseline=true` to copy that report into `latest.json`, append a dated
history entry, and open a reviewable pull request. The workflow never pushes a
baseline directly to the default branch.
