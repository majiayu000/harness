# Harness TypeScript SDK

TypeScript client for the Harness JSON-RPC app server.

## Install

```bash
npm install harness-sdk
```

## Usage

```ts
import { Harness } from "harness-sdk";

const harness = new Harness({ baseUrl: "http://127.0.0.1:9800", cwd: "/repo" });
const thread = await harness.startThread();

const result = await thread.run("Summarize the repository", {
  onEvent: (event) => {
    console.log(event.method, event.params);
  },
});

console.log(result.status, result.output);
```

### Authenticated server

When the server is configured with `api_token`, pass `apiToken`:

```ts
const harness = new Harness({
  baseUrl: "http://127.0.0.1:9800",
  cwd: "/repo",
  apiToken: process.env.HARNESS_API_TOKEN,
});
```

### Stream events explicitly

```ts
for await (const event of thread.runStream("Diagnose failing tests")) {
  console.log(event.method, event.params);
}
```

Events are SDK-synthesized polling lifecycle events:
`sdk:turn/started`, `sdk:turn/status`, `sdk:turn/completed`, `sdk:turn/timeout`.

Timeout events use `event.params.timeout_seconds`.
Turn snapshots preserve server fields such as `approval_request.id`.

## Publish to npm

```bash
npm run build
npm publish --access public
```
