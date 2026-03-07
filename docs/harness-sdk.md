# Harness SDKs (TypeScript + Python)

Harness now includes first-party SDK scaffolds for programmatic JSON-RPC usage.

## Scope

- TypeScript SDK with `startThread()`, `resumeThread()`, `thread.run(prompt)`, and `thread.runStream(prompt)`.
- Python SDK with `start_thread()`, `resume_thread()`, `thread.run(prompt)`, and `thread.run_stream(prompt)`.
- Event streaming via incremental run events (`turn/started`, `turn/status`, `turn/completed`, `turn/timeout`).

## Package Locations

- TypeScript: `sdk/typescript`
- Python: `sdk/python`

## Quick Usage

### TypeScript

```ts
import { Harness } from "harness-sdk";

const harness = new Harness({ baseUrl: "http://127.0.0.1:8080" });
const thread = await harness.startThread();
const result = await thread.run("Analyze this repository");
```

### Python

```python
from harness_sdk import Harness

harness = Harness(base_url="http://127.0.0.1:8080")
thread = harness.start_thread()
result = thread.run("Analyze this repository")
```

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
