# Product Spec

## Linked Issue

GH-1416

## User Problem

An operator who deploys harness without setting `HARNESS_API_TOKEN` (or the `api_token` config key) gets a server whose entire mutating API surface — task creation, cancellation, reconciliation, merge endpoints — is open to anyone who can reach the port. Nothing warns them. Because harness spawns code agents and can write to GitHub repositories, an unauthenticated server is effectively remote code execution for the local network. Security posture should not depend on remembering a config key.

## Goals

- A forgotten token can no longer silently produce an unauthenticated server.
- Deliberate tokenless operation (local dev) remains possible with one explicit config line.
- Operators get an actionable message telling them exactly what to set.

## Non-Goals

- New authentication schemes, users, or roles (stays a single bearer token).
- Changes to webhook/signal HMAC verification or to the auth-exempt dashboard/static routes.
- Network-level protections (bind address, TLS) — out of scope.

## User-Visible Behavior

- Starting the server with no API token and no explicit opt-in fails fast at startup with an error naming both the config key and the environment variable, plus the `allow_unauthenticated = true` escape hatch.
- Starting with `allow_unauthenticated = true` and no token works like today, but logs a prominent startup warning that the API is unauthenticated.
- Starting with a token behaves exactly as today (constant-time bearer check on non-exempt routes).

## Acceptance Criteria

- [ ] No token + no opt-in: server refuses to start; error message names `api_token`, `HARNESS_API_TOKEN`, and `allow_unauthenticated`.
- [ ] No token + `allow_unauthenticated = true`: server starts, warns at startup, middleware passes through.
- [ ] Token configured: enforcement unchanged (including the SSE `?token=` path and exempt routes).
- [ ] README and usage docs describe the new default and the opt-in.

## Edge Cases

- Token set to an empty or whitespace-only string: treated as "no token configured", not as a valid empty token.
- Both token and `allow_unauthenticated = true` set: token wins; enforcement is active and a warning notes the ignored opt-in.
- Existing deployments upgrading without reading release notes: startup failure is the intended signal; the error message must make recovery a one-line change.

## Rollout Notes

This is a deliberate breaking default for tokenless deployments. Release note must carry an upgrade callout with the two recovery paths (set a token, or opt in explicitly). No data migration.
