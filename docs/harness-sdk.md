# Harness SDKs (TypeScript + Python)

Harness includes first-party SDKs for programmatic workflow-runtime submissions.

## Scope

- TypeScript SDK with local project handles via `startThread()` / `resumeThread()` and runtime-backed `thread.run(prompt)` / `thread.runStream(prompt)`.
- Python SDK with local project handles via `start_thread()` / `resume_thread()` and runtime-backed `thread.run(prompt)` / `thread.run_stream(prompt)`.
- Event streaming via SDK lifecycle events (`sdk:turn/started`, `sdk:turn/status`, `sdk:turn/completed`, `sdk:turn/timeout`).

Each run creates a durable submission with `POST /api/workflows/runtime/submissions`,
polls `GET /api/workflows/runtime/submissions/{id}`, and reads terminal output
from the submission's artifacts. The thread-shaped SDK object is a local
project-scoped convenience handle; Harness no longer exposes server-side thread
or turn lifecycle RPCs.

## Package Locations

- TypeScript: `sdk/typescript`
- Python: `sdk/python`

## Quick Usage

### TypeScript

```ts
import { Harness } from "harness-sdk";

const harness = new Harness({ baseUrl: "http://127.0.0.1:9800", cwd: "/repo" });
const thread = await harness.startThread();
const result = await thread.run("Analyze this repository");
```

### Python

```python
from harness_sdk import Harness

harness = Harness(base_url="http://127.0.0.1:9800", cwd="/repo")
thread = harness.start_thread()
result = thread.run("Analyze this repository")
```

Python SDK polling is synchronous/blocking; use it from non-async contexts or run it in a worker thread.

The SDK event names are intentionally prefixed (`sdk:*`) to distinguish synthesized polling events from raw server-side notification method names.

## Publish Notes

### npm

```bash
cd sdk/typescript
npm run build
npm publish --access public
```

### PyPI

```bash
cd sdk/python
python -m build
twine upload dist/*
```
