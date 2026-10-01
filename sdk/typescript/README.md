# Harness TypeScript SDK

TypeScript client for Harness workflow-runtime submissions.

## Install

Install this SDK from a checkout of [majiayu000/harness](https://github.com/majiayu000/harness),
using Node.js 20+ and npm. The public npm package named `harness-sdk` is a
different project; `npm install harness-sdk` does not install this client.

```bash
git clone https://github.com/majiayu000/harness.git
cd harness/sdk/typescript
npm install
npm run build
npm pack
```

The current package version produces `harness-sdk-0.1.0.tgz`, containing the
built JavaScript and type declarations. From your application's directory,
install that local archive (replace the path with your checkout):

```bash
npm install /absolute/path/to/harness/sdk/typescript/harness-sdk-0.1.0.tgz
```

The usage examples require a running [Harness server](../../README.md#level-up-the-fleet-control-plane).
Installing the SDK does not build or start the server.

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

`startThread()` creates a local project-scoped handle. Each `run()` call submits a
new prompt through `POST /api/workflows/runtime/submissions` and polls the durable
runtime submission until it is terminal. `resumeThread(project)` reconstructs the
same local handle; it does not restore removed server-side thread history.

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
