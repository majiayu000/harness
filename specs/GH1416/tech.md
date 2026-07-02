# Tech Spec

## Linked Issue

GH-1416

## Product Spec

See `specs/GH1416/product.md`.

## Current System

- `api_auth_middleware` (`crates/harness-server/src/http/auth.rs:149`) is layered globally in `http_router.rs:176`. When no token is configured it returns pass-through: `auth.rs:162-165` ("skip auth for backward compatibility").
- Token comparison uses constant-time `ct_eq` (`auth.rs:192`). Exempt paths are listed at `auth.rs:114` (health, webhooks/signals with their own HMAC, dashboard/static, `/ws` with its own origin+bearer check). SSE streams may pass the token via `?token=` (`auth.rs:178`).
- Token sourcing: `api_token` config / `HARNESS_API_TOKEN` env (resolved in server config loading).

## Proposed Design

1. Add `allow_unauthenticated: bool` (default `false`) to the server HTTP config alongside `api_token`.
2. Move the decision to startup, not per-request: during server construction (config validation in `http/mod.rs` startup path), evaluate:
   - token present (non-empty after trim) -> `AuthMode::Enforced`
   - no token + `allow_unauthenticated = true` -> `AuthMode::Open` + `warn!` (single prominent startup log)
   - no token + no opt-in -> return a startup error (server refuses to bind) with the actionable message.
3. `api_auth_middleware` keeps its current shape but branches on the resolved `AuthMode` instead of re-checking token presence; `Open` behaves exactly like today's pass-through.
4. Normalize empty/whitespace tokens to `None` at config load so "empty string" cannot satisfy `Enforced` mode.
5. If both token and opt-in are set, log that `allow_unauthenticated` is ignored.

## Data Flow

- Inputs: server config file + `HARNESS_API_TOKEN` env at startup.
- Outputs: resolved `AuthMode` stored in `AppState`/router layer state; startup log line stating the mode; startup error on misconfiguration.
- No persistence changes, no external calls.

## Alternatives Considered

- Refuse only mutating routes while serving read-only ones unauthenticated: more complex route classification for little benefit; read endpoints also leak task/repo data. Rejected.
- Warn-only (no startup failure): preserves the silent-failure mode this change exists to eliminate. Rejected.
- Auto-generate a random token and print it at startup: friendlier for dev, but printed secrets end up in logs; may be a later dev-mode convenience, out of scope here.

## Risks

- Security: strictly improves; no new surface.
- Compatibility: breaking for tokenless deployments — intended; release-note callout plus a one-line recovery.
- Performance: none (decision moves to startup).
- Maintenance: small; auth logic becomes more explicit via `AuthMode`.

## Test Plan

- [ ] Unit tests: config resolution matrix (token / empty token / opt-in / both) -> `AuthMode` or startup error; middleware behavior per mode.
- [ ] Integration tests: server construction fails with no token and no opt-in; starts with opt-in and serves unauthenticated; enforces 401 with token set (reuse existing auth tests at `http/auth.rs` test module).
- [ ] Manual verification: run `harness serve` in all three configurations and capture the startup output.

## Rollback Plan

Setting `allow_unauthenticated = true` restores the previous behavior per deployment without code changes. Full rollback is reverting the change; no data migration involved either way.
