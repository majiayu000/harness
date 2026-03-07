# Harness Python SDK

Python client for the Harness JSON-RPC app server.

## Install

```bash
pip install harness-sdk
```

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

### Stream events explicitly

```python
for event in thread.run_stream("Diagnose failing tests"):
    print(event["method"], event["params"])
```

Events are SDK-synthesized polling lifecycle events:
`sdk:turn/started`, `sdk:turn/status`, `sdk:turn/completed`, `sdk:turn/timeout`.

This SDK uses synchronous polling; calls block the current thread.

## Publish to PyPI

```bash
python -m build
twine upload dist/*
```
