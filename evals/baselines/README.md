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

Set `HARNESS_EVAL_PROJECT_ROOT` to the server's existing project root on the
isolated runner. The CLI dispatches through PostgreSQL, not through
`HARNESS_EVAL_SERVER_URL`: that URL is used only by preflight. CLI and server
must use the same database and that project's `WORKFLOW.md` storage namespace.
The runner must be able to read the project root. A reachable HTTP server alone
does not establish that it will dispatch the CLI's workflows.

For model activities, set `HARNESS_EVAL_CREDENTIAL_FILE` in the **server**
environment to a private operator-owned JSON file. It contains the existing
`credential_requirements` and `credential_grants` arrays; each grant declares
its requirement ID, environment variable, issuer, scope, audience, expiration,
and value. Use a short-lived provider credential such as `OPENAI_API_KEY`, with
an audience and scope matching its declared requirement. Create this file with
mode `0600` outside the checkout and remove it after the run. Never commit it,
upload it as an artifact, or put its contents in shell arguments.

The control plane reads the file at the model-job claim boundary and reuses the
existing grant validation. Expired, missing-required, conflicting, malformed,
or unreadable grants fail the claim. Only grant metadata enters the job's audit;
values travel in the authenticated claim response and host process stdin.
Native quality gates ignore this operator file and retain offline verification.
Without the file, the original empty-by-default credential policy remains.
The fixed manifests explicitly authorize the Codex provider and GitHub hosts;
other providers require a deliberately edited manifest and a new suite identity.
An interactive ChatGPT login is not a provider API-key grant for this formal
host. See the [supervised host limits](../../docs/references/supervised-docker-host.md)
before enabling nightly; these prerequisites alone do not supply a complete
multi-activity suite executor or offline Cargo dependencies.

If a report records `event_persistence_failed`, repair the observe stream with
`harness eval retry-events <report.json>`. The command re-emits deterministic
event IDs and atomically clears only the event-persistence outcome after the
write succeeds. Incomplete reports are rejected by `eval diff` and baseline
refresh eligibility checks.

After a green report-only run, manually dispatch the workflow with
`refresh_baseline=true` to copy that report into `latest.json`, append a dated
history entry, and open a reviewable pull request. The workflow never pushes a
baseline directly to the default branch.
