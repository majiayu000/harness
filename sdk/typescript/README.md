# Harness TypeScript SDK

TypeScript client for the Harness JSON-RPC app server.

## Install

```bash
npm install harness-sdk
```

## Usage

```ts
import { Harness } from "harness-sdk";

const harness = new Harness({ baseUrl: "http://127.0.0.1:8080" });
const thread = await harness.startThread();

const result = await thread.run("Summarize the repository", {
  onEvent: (event) => {
    console.log(event.method, event.params);
  },
});

console.log(result.status, result.output);
```

### Stream events explicitly

```ts
for await (const event of thread.runStream("Diagnose failing tests")) {
  console.log(event.method, event.params);
}
```

## Publish to npm

```bash
npm run build
npm publish --access public
```
