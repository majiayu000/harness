# Harness Python SDK

Python client for Harness workflow-runtime submissions.

## Install

Install this SDK from a checkout of [majiayu000/harness](https://github.com/majiayu000/harness)
with Python 3.9+ in a virtual environment. The public PyPI package named
`harness-sdk` is not a verified distribution of this client; use the local
source path instead of `pip install harness-sdk`.

```bash
git clone https://github.com/majiayu000/harness.git
cd harness
python3 -m venv .venv
. .venv/bin/activate
python -m pip install ./sdk/python
```

This installs the local `harness-sdk` distribution and its `harness_sdk` module
into that environment. Keep the environment active when running the examples.

The usage examples require a running [Harness server](../../README.md#level-up-the-fleet-control-plane).
Installing the SDK does not build or start the server.

## Usage

```python
from harness_sdk import Harness

harness = Harness(base_url="http://127.0.0.1:9800", cwd="/repo")
thread = harness.start_thread()

result = thread.run(
    "Summarize the repository",
    on_event=lambda event: print(event["method"], event["params"]),
)

print(result.status, result.output)
```

`start_thread()` creates a local project-scoped handle. Each `run()` call submits
a new prompt through `POST /api/workflows/runtime/submissions` and polls the
durable runtime submission until it is terminal. `resume_thread(project)`
reconstructs the same local handle; it does not restore removed server-side
thread history.

### Authenticated server

When the server is configured with `api_token`, pass `api_token`:

```python
harness = Harness(
    base_url="http://127.0.0.1:9800",
    cwd="/repo",
    api_token="your-token",
)
```

### Stream events explicitly

```python
for event in thread.run_stream("Diagnose failing tests"):
    print(event["method"], event["params"])
```

Events are SDK-synthesized polling lifecycle events:
`sdk:turn/started`, `sdk:turn/status`, `sdk:turn/completed`, `sdk:turn/timeout`.

This SDK uses synchronous polling; calls block the current thread.
