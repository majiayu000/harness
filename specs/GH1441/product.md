# Product Spec

## Linked Issue

GH-1441

## User Problem

Operators can configure GitHub intake for `webhook` or `hybrid` mode without a
usable `server.github_webhook_secret`. The server starts, logs that webhook
requests will be refused, and may still report `/health` as `ok`. In `webhook`
mode this means GitHub issue intake is effectively off. In `hybrid` mode this
means the configured webhook path is unavailable and polling silently carries
the load.

The resulting deployment is self-contradictory: the configured intake mode says
webhooks should be accepted, but the only visible operator surface that notices
the problem is the log stream.

## Goals

- Validate GitHub intake mode against webhook secret availability during server
  startup.
- When GitHub `webhook` or `hybrid` intake is enabled without a non-empty
  webhook secret, make the server health degraded instead of fully healthy.
- Keep webhook requests fail-closed when the secret is absent or invalid.
- Make the effective GitHub intake mode and active drivers queryable from the
  intake status surface.
- Avoid exposing secret values or raw secret-adjacent error strings in API
  responses.

## Non-Goals

- Changing webhook signature verification semantics.
- Changing GitHub issue polling scheduling, sprint planning, or auto-merge
  behavior.
- Adding per-repo intake mode configuration; the current GitHub intake mode
  remains global.
- Introducing a new persistence table for intake configuration health.
- Disabling all polling when only the webhook driver is misconfigured in
  `hybrid` mode.

## User-Visible Behavior

When GitHub intake is enabled with mode `webhook` or `hybrid`, startup validates
that `server.github_webhook_secret` is present and non-empty after trimming. If
the secret is missing, blank, or whitespace-only, the server may continue to run
so operators can inspect it, but `/health` reports `status: degraded` and names
the GitHub webhook intake subsystem as degraded.

Webhook requests continue to fail closed with a non-success response while the
secret is unavailable. The response must not expose secret values.

`GET /api/intake` exposes enough GitHub channel detail for operators to see the
configured mode, whether webhook intake is currently accepting events, whether
polling is active, and the effective per-repo entries derived from the single
repo shorthand plus the `repos` array. In `webhook` mode with no usable secret,
the GitHub channel is visible as configured but its webhook driver is degraded
instead of looking like a working intake path.

Poll-only GitHub intake does not require a webhook secret. Missing webhook
secret configuration must not degrade health when GitHub intake is disabled or
when the enabled GitHub mode is `poll`.

## Acceptance Criteria

- [ ] Startup validation treats a missing, empty, or whitespace-only
      `server.github_webhook_secret` as unusable for GitHub `webhook` and
      `hybrid` intake modes.
- [ ] GitHub `webhook` mode without a usable secret starts in a degraded health
      state or fails startup explicitly; if the server starts, `/health` reports
      `status: degraded`.
- [ ] GitHub `hybrid` mode without a usable secret reports degraded health while
      preserving the polling driver when it is otherwise available.
- [ ] GitHub `poll` mode and disabled GitHub intake do not require
      `server.github_webhook_secret` and do not degrade health for that reason.
- [ ] `GET /api/intake` reports the configured GitHub intake mode and active
      drivers, including webhook acceptance state and polling state.
- [ ] `GET /api/intake` reports effective GitHub repo entries after applying the
      single `repo` shorthand and `repos` array merge rules.
- [ ] API responses redact secret values and avoid raw secret-adjacent startup
      error text.
- [ ] Tests cover webhook-only, hybrid, poll-only, disabled, blank secret, and
      whitespace-only secret cases.

## Edge Cases

- A TOML value of `github_webhook_secret = ""` is invalid for webhook-capable
  GitHub intake.
- A whitespace-only secret is invalid even if deserialization produces
  `Some(value)`.
- An empty `GITHUB_WEBHOOK_SECRET` environment variable does not override a
  configured non-empty TOML secret and must not create a false degradation.
- Hybrid mode may still have a functioning poller; health should be degraded
  because one configured intake driver is unavailable, not because all intake is
  stopped.
- If the workflow runtime store is unavailable, `/api/intake` may already report
  partial runtime submission data; the webhook-secret degradation must compose
  with that existing degraded response.

## Rollout Notes

This is an operator-surface correctness fix. It changes startup validation and
status reporting for self-contradictory GitHub intake configuration. Existing
poll-only deployments are compatible. Webhook or hybrid deployments without a
secret will become visibly degraded until `server.github_webhook_secret` or
`GITHUB_WEBHOOK_SECRET` is configured with a non-empty value.
