# harness-web

React SPA consumed by `crates/harness-server` via `build.rs`.
Routes `/` (dashboard) and `/overview` both return `dist/index.html`;
the client router dispatches on `location.pathname`.

## Dev

Run a backend in one terminal:

```sh
cargo run -p harness-cli -- serve --transport http --port 9800 --project-root .
```

Then in this directory:

```sh
bun install
bun run dev      # http://localhost:5173
```

Vite proxies `/api/*`, `/health`, `/ws` to the backend.

## Build

`bun run build` emits `dist/`. Cargo build picks it up automatically.
Do not edit `dist/` — it is generated.

## Test

```sh
bun run typecheck
bun run test
```
