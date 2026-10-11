# Try common tasks before upgrading an Agent

Use the existing historical replay manifest, `eval run`, adapter tests, and
`eval diff`. Start with one real task, then run the same frozen task set with
the installed version and the candidate version. The comparison entry point is:

```sh
bash scripts/compare-agent-upgrade.sh "$HARNESS_BIN" \
  "$TRIAL/baseline.json" "$TRIAL/candidate.json" "$TRIAL/diff.json"
```

The helper checks both reports with the existing baseline eligibility checker,
then runs the existing diff with zero tolerated pass-rate drop and the new F-gate
regression gate. It propagates failures. Failed task results can be compared;
pending, skipped, interrupted, or infrastructure-incomplete runs cannot. This
does not promote a report to a reviewed nightly baseline.

## Choose the repository and tasks

Set absolute paths in an operator shell. `SOURCE` is the user's repository;
`TRIAL` is a private evidence directory outside that repository. Use a dedicated
disposable server and PostgreSQL database, with a Harness config whose
`server.project_root` is `SOURCE`. The server and CLI must use the same database
and the same project's workflow schema. Follow the existing
[server launcher](../../scripts/start-harness-codex-safe.sh) and
[runtime host guide](supervised-docker-host.md) to prepare that environment.
Supply credentials through the configured provider/credential service; do not
put secrets in the manifest, command arguments, or committed evidence.

```sh
HARNESS_BIN=/absolute/path/to/harness
SOURCE=/absolute/path/to/user-repository
TRIAL=/absolute/path/to/private-upgrade-trial
CONFIG=/absolute/path/to/disposable-server.toml
MANIFEST="$TRIAL/common-tasks.toml"
mkdir -p "$TRIAL"
chmod 700 "$TRIAL"
```

Select a few resolved GitHub issues that represent actual common work. The
current manifest contract takes repository/issue pairs, frozen base commits,
and native verification commands; it does not take arbitrary free-text tasks.
Use the [existing manifest contract](../../evals/benchmarks/README.md). For
example, write this to `MANIFEST`, replacing the repository, issue, base SHA,
and verification command with the user's real case:

```toml
schema_version = 1
suite = "common-agent-tasks"
default_timeout_secs = 300

[[cases]]
case_id = "common-fix"
repo = "OWNER/REPOSITORY"
issue = 123
base_commit = "FULL_40_CHARACTER_LOWERCASE_BASE_SHA"
verify_commands = ["cargo test -p ACTUAL_PACKAGE ACTUAL_BEHAVIOR_FILTER"]
```

Prove that the independent acceptance check rejects the unchanged base and
accepts the known resolution. Keep those results. Provide offline dependencies
in the verifier image; candidate-owned tests alone are not evidence that an
issue was fixed. The two runs must use this exact same manifest, commands,
resource policy, verifier, Harness version, and `k`. Changing the task set or
timeout policy starts a different comparison: `eval diff` enforces schema and
suite digest identity. Do not change the manifest's isolation image between
the two runs to select the Agent version.

```sh
"$HARNESS_BIN" --config "$CONFIG" eval run \
  --manifest "$MANIFEST" --dry-run --k 1 --json \
  --output "$TRIAL/tasks.json"
```

Dry-run validates the task contract; it does not run an Agent or establish a
baseline. With `k = 1`, this trial reports one execution per task per version.
Increasing `k` changes the statistical estimate, not the number of executions.

## Select the actual Agent version

`eval run --config` selects the workflow store and project. It does **not**
select the executable or model used by a `remote_host`. Configure the actual
runtime host for each phase and retain its version evidence.

For the existing supervised Docker host, the supported Agent is Codex via
`codex exec`. Pin two images with different CLI package versions using the
existing Dockerfile, and retain the observed version from each image:

```sh
OLD_CODEX_VERSION=YOUR_INSTALLED_VERSION
CANDIDATE_CODEX_VERSION=YOUR_CANDIDATE_VERSION
docker build -f docker/agent/Dockerfile \
  --build-arg "CODEX_CLI_PACKAGE=@openai/codex@$OLD_CODEX_VERSION" \
  -t harness-agent-upgrade-old .
docker build -f docker/agent/Dockerfile \
  --build-arg "CODEX_CLI_PACKAGE=@openai/codex@$CANDIDATE_CODEX_VERSION" \
  -t harness-agent-upgrade-candidate .
OLD_IMAGE=$(docker image inspect --format '{{.Id}}' harness-agent-upgrade-old)
CANDIDATE_IMAGE=$(docker image inspect --format '{{.Id}}' harness-agent-upgrade-candidate)
docker run --rm --network none "$OLD_IMAGE" codex --version > "$TRIAL/old-version.txt"
docker run --rm --network none "$CANDIDATE_IMAGE" codex --version > "$TRIAL/candidate-version.txt"
```

Keep the same model when measuring a CLI upgrade. For a model upgrade, keep the
same CLI image and change the host's `--model` instead; identify the trial as a
model comparison. The host persists image/model identity in each claim's
`state.json`. An image tag or a run name alone is not proof of the version used.

Run the existing adapter regression tests once from the Harness checkout:

```sh
cargo test -p harness-agents codex_adapter
cargo test -p harness-agents opencode_adapter
```

These tests exercise Harness protocol handling, mostly with stubs. They do not
test the two installed CLI versions on the user's tasks. OpenCode and Claude
also need their own capable runtime host for live comparisons; the supervised
Codex host does not select either Agent by changing a config label.

## Execute baseline, then candidate

Start the baseline run in one terminal. In another, service its runtime jobs
using the old image. Run the candidate phase only after the baseline's workflow
family and containers have finished cleanup; service it with the candidate
image. Use a fresh run ID and output path for every attempt. Both commands wait
for the existing workflow runtime and preserve partial evidence on failure.

```sh
"$HARNESS_BIN" --config "$CONFIG" eval run --manifest "$MANIFEST" \
  --execute --k 1 --run-id upgrade-old-ATTEMPT_ID \
  --dispatch-timeout-secs 120 --json --output "$TRIAL/baseline.json"

# After baseline cleanup, and with the candidate host selected:
"$HARNESS_BIN" --config "$CONFIG" eval run --manifest "$MANIFEST" \
  --execute --k 1 --run-id upgrade-candidate-ATTEMPT_ID \
  --dispatch-timeout-secs 120 --json --output "$TRIAL/candidate.json"
```

The supervised host handles **one claim per process**, not a complete eval
suite. For each claim, use the existing command with the current case's frozen
source, exact full base SHA, request/submission identity, and a fresh state
directory. `IMAGE` must be `OLD_IMAGE` for every baseline model activity, then
`CANDIDATE_IMAGE` for every candidate model activity. Keep `MODEL` fixed for a
CLI comparison. `VERIFIER_IMAGE` and `PROXY_IMAGE` are independently pinned
local image IDs prepared as described in the runtime host guide:

```sh
python3 scripts/run-supervised-docker-host.py \
  --server-url "$SERVER_URL" --request "$REQUEST_JSON" \
  --submission "$SUBMISSION_JSON" --workspace "$SOURCE" \
  --base-commit "$CASE_BASE_SHA" --verifier "$INDEPENDENT_VERIFIER" \
  --auth-file "$PRIVATE_AUTH_FILE" --state-dir "$CLAIM_STATE" \
  --image "$IMAGE" --verifier-image "$VERIFIER_IMAGE" \
  --proxy-image "$PROXY_IMAGE" --model "$MODEL" --timeout 300
```

For formal eval claims, the host uses the control-plane credential grant,
**not** `--auth-file`. A missing grant is a real authentication blocker.
For subsequent review/fix activities and native `run_quality_gate`, copy the
prior claim's retained `candidate/` into a fresh state directory as described
under [candidate handoff](supervised-docker-host.md#native-quality-gate-consumption).
Retain the candidate across all activities; do not start review from the base.
The native gate must match the server-selected `expected_head_sha` and runs
the exact validation argv in the offline verifier without model credentials.

The supervised host currently rejects pinned Agent contracts and exact replay,
has a 7200-second CLI timeout ceiling (the default remains 180 seconds), and does not implement PR lifecycle
actions. A task requiring those actions needs an existing capable runtime
host; it cannot be called complete merely because implementation and review
passed. There is no automatic multi-claim worker in this entry point. Record
unsupported or unserviced claims as blocked, retain partial reports, and fix
the actual host integration before comparing full runs.

## Read the result

Run the comparison command at the top of this page after both live reports are
complete. `diff.json` contains case transitions, newly failing gates,
verification evidence, source commits, terminal states, pass-rate deltas,
tokens, and cost deltas. A regression writes the diff and returns nonzero.
Unknown provider costs stay `null`/`unknown`; missing resource measurements
remain incomplete rather than zero. Keep all failed attempts instead of
overwriting them with a later successful report.

Alongside each report, retain version/image/model pins, prompts and host state,
native verifier output, elapsed start/end times, failed attempts, retries,
manual interventions (what the operator had to inspect or change), and rework
cycles. Compare those counts per case across versions. Eval reports do not
automatically measure human attention, so a pass-rate change alone cannot
establish fewer checks or less rework. Without a comparable baseline, report
the observed task results and leave the improvement claim unresolved.
