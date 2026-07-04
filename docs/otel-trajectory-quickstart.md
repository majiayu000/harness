# OTel Trajectory Quickstart

This quickstart runs a local Grafana OTel LGTM stack and sends Harness workflow
trajectory spans to Tempo. It is intended for development and demo use.

## Start the Backend

```bash
docker compose -f docs/otel-trajectory-compose.yml up -d
```

The compose snippet exposes Grafana on `http://127.0.0.1:3000`, OTLP gRPC on
`http://127.0.0.1:4317`, and OTLP HTTP on `http://127.0.0.1:4318`.

## Configure Harness

Build the server and create a local config file:

```bash
cargo build --release
cat > /tmp/harness-otel-quickstart.toml <<'TOML'
[server]
transport = "http"
http_addr = "127.0.0.1:9800"
data_dir = ".harness/otel-quickstart"
allow_unauthenticated = true

[agents]
default_agent = "auto"
sandbox_mode = "danger-full-access"

[agents.codex]
cli_path = "codex"

[otel]
environment = "local"
exporter = "otlp-http"
endpoint = "http://127.0.0.1:4318"
trajectory = true
capture_content = false
log_user_prompt = false

[[projects]]
name = "target-repo"
root = "/absolute/path/to/target-repo"
default = true
TOML
```

Replace `root` with the repository that should receive the issue run. Keep
`capture_content = false` unless the operator has explicitly approved exporting
prompt or response content to the backend.

Start Harness:

```bash
./target/release/harness --config /tmp/harness-otel-quickstart.toml serve \
  --transport http \
  --port 9800
```

## Submit an Issue Run

Submit a real issue-backed task in a repository where the configured agent can
open a PR:

```bash
curl -X POST http://127.0.0.1:9800/tasks \
  -H "Content-Type: application/json" \
  -d '{
    "project": "/absolute/path/to/target-repo",
    "issue": 123,
    "description": "otel trajectory smoke run"
  }'
```

Poll the task until it reaches a terminal state:

```bash
curl -s http://127.0.0.1:9800/tasks/{task_id} | python3 -m json.tool
```

## Inspect the Trace

Open Grafana at `http://127.0.0.1:3000`, go to Explore, choose the Tempo data
source, and search for Harness traces with:

```traceql
{ resource.service.name = "harness-observe" }
```

A complete issue-to-PR run should show one `harness.workflow` root span with
`harness.activity` child spans and `gen_ai.client.operation` turn spans. The
turn spans carry model, input token, output token, cost, workflow id, activity
kind, runtime job id, thread id, turn id, outcome, and retry-attempt attributes
when those values are known. Unknown token or cost values are omitted instead of
zero-filled.

## Manual Verification Record

For a PR walkthrough, record:

- `docker compose -f docs/otel-trajectory-compose.yml up -d`
- the `[otel]` config values used for the run
- the submitted task id, workflow id, issue number, and PR URL
- the Tempo query result showing `harness.workflow`, `harness.activity`, and
  `gen_ai.client.operation` spans in one trace
- `docker compose -f docs/otel-trajectory-compose.yml down`

## Troubleshooting

- If Harness fails startup with an unreachable OTLP endpoint, make sure the
  compose stack is running and that port `4318` is not blocked.
- If Grafana has no trace, wait for the workflow-runtime job to complete and
  then refresh Explore. Spans are emitted after committed runtime job
  completion.
- If only metrics or logs appear, confirm `exporter = "otlp-http"` and
  `trajectory = true` in the config actually used by the server process.
