# Tech Spec

## Linked Issue

GH-1441

## Product Spec

See `specs/GH1441/product.md`.

## Current System

| Area | Files | Current behavior | Why relevant |
| --- | --- | --- | --- |
| GitHub intake config | `crates/harness-core/src/config/intake.rs` | `GitHubIntakeConfig` has global `enabled` and `mode` fields. `IntakeMode::webhook_autonomous()` is true for `Webhook` and `Hybrid`; `poller_enabled()` is true for `Poll` and `Hybrid`. | This is the source of truth for whether a webhook secret is required. |
| Webhook secret config | `crates/harness-core/src/config/server.rs` | `ServerConfig.github_webhook_secret` is optional. Env override ignores empty `GITHUB_WEBHOOK_SECRET`, but TOML can still produce `Some("")` or whitespace. Debug output redacts configured values. | Startup validation must use normalized secret availability without leaking values. |
| App state startup | `crates/harness-server/src/http/init.rs` | `build_app_state` logs a warning when the webhook secret is absent or empty, regardless of GitHub intake mode, but does not record degraded health. | This is where the startup validation and health degradation should be wired. |
| GitHub poller construction | `crates/harness-server/src/http/builders/intake.rs` | Pollers are created only when GitHub intake is enabled and the mode allows polling. Webhook mode correctly creates no poller. | The fix must preserve poller behavior, especially for hybrid mode. |
| Webhook route | `crates/harness-server/src/http/misc_routes.rs` | `github_webhook` fails closed when the secret is missing or exactly empty. Whitespace-only secrets are currently accepted as a configured value. | Route behavior should remain fail-closed and use the same secret usability rule as startup validation. |
| Health route | `crates/harness-server/src/http/misc_routes.rs` | `/health` reports degraded when `degraded_subsystems` is non-empty, runtime state is dirty, circuit breakers open, or isolation is unavailable. | Startup validation can surface the misconfiguration through existing health machinery. |
| Intake status route | `crates/harness-server/src/http/misc_routes.rs` | `/api/intake` reports GitHub channel `enabled`, one `repo`, active count, and recent dispatches, but not mode, driver state, or effective repos. | This is the operator surface requested by the issue. |

## Proposed Design

1. Add a small normalized GitHub webhook-secret validation helper.
   - Treat `None`, empty string, and whitespace-only string as unusable.
   - Treat only trimmed non-empty values as usable.
   - Keep the helper value-oriented so tests can cover it without starting the
     server.
2. Evaluate GitHub intake mode during `build_app_state`.
   - If `intake.github` is absent, disabled, or mode is `poll`, do not require a
     webhook secret.
   - If GitHub intake is enabled and mode is `webhook` or `hybrid`, require the
     normalized secret to be usable.
   - When the requirement is not met, append a sanitized optional startup status
     and include a stable degraded subsystem name such as
     `github_webhook_intake`.
   - Continue server startup so `/health` and `/api/intake` remain available for
     operators.
3. Align the webhook handler with the normalized helper.
   - Missing, empty, and whitespace-only secrets return the existing fail-closed
     non-success response class.
   - Do not include secret values in the response body or logs.
4. Expand `GET /api/intake` GitHub channel metadata.
   - Preserve existing top-level channel fields, including `name`, `enabled`,
     `repo`, and `active`, for compatibility.
   - Add `mode` using the configured `IntakeMode` string.
   - Add driver state for `webhook` and `polling`, including whether each driver
     is configured by mode and whether it is currently active or degraded.
   - Add effective repo entries from `GitHubIntakeConfig::effective_repos()`,
     including `repo`, `label`, optional `project_root`, and the effective
     driver mode for that repo.
   - If runtime submission summaries are already degraded, compose the existing
     partial response with the webhook-secret driver degradation rather than
     replacing it.
5. Update tests.
   - Unit-test the normalized secret rule.
   - Add focused startup/health tests for webhook, hybrid, poll, disabled, empty
     secret, and whitespace-only secret cases.
   - Add intake status route tests for mode, driver state, and effective repos.
   - Add webhook route regression coverage for whitespace-only secret handling.

## Data Flow

Configuration is loaded into `HarnessConfig`. During app-state construction,
the startup validation reads `config.intake.github` and
`config.server.github_webhook_secret`. It produces a normalized secret
availability result and a GitHub intake driver status. If webhook-capable GitHub
intake lacks a usable secret, `build_app_state` records a sanitized degraded
subsystem before constructing `AppState`.

`/health` reads the existing `degraded_subsystems` and `startup_statuses` from
`AppState`. No secret value crosses into the health response; the response only
names the degraded subsystem and uses the existing redacted startup error code.

`/api/intake` reads the configured GitHub intake mode, effective repos, live
poller registrations from `state.intake.github_pollers` and
`github_poller_repos`, and the normalized secret availability. It reports which
drivers are configured and currently usable. Runtime submission counts and
recent dispatches continue to flow from the existing task and workflow-runtime
queries.

The webhook route reads the same normalized secret availability before
signature verification. If the secret is unusable, the request fails before any
payload dispatch logic runs.

## Alternatives Considered

- Fail server startup for webhook-capable GitHub intake without a secret.
  Rejected for this issue because a degraded-but-running server keeps `/health`
  and `/api/intake` available for operators and matches the issue title.
- Keep logging only. Rejected because the problem is specifically invisible
  healthy status despite a configured intake driver being unavailable.
- Disable hybrid polling when the webhook secret is missing. Rejected because
  hybrid mode intentionally keeps polling as a backstop; the correct behavior is
  visible degradation, not removing the remaining working driver.
- Add a separate `/api/intake/repos` endpoint. Rejected because the existing
  intake status route already owns intake operator status.

## Risks

- Security: secret values must remain redacted in Debug output, health output,
  intake status output, webhook errors, and logs.
- Compatibility: clients that parse `/api/intake` should continue to find the
  existing channel fields. New fields should be additive.
- Performance: startup validation is local config inspection only. Intake status
  should reuse existing in-memory poller and config data and avoid new network
  calls.
- Maintenance: driver-state computation should be centralized enough that
  health and intake status do not drift on the definition of usable webhook
  secret.

## Test Plan

- [ ] Unit tests: normalized webhook secret availability treats missing, empty,
      and whitespace-only values as unusable and preserves non-empty values.
- [ ] Startup/health tests: GitHub webhook mode without a usable secret reports
      degraded health.
- [ ] Startup/health tests: GitHub hybrid mode without a usable secret reports
      degraded health while preserving polling registration when available.
- [ ] Startup/health tests: GitHub poll mode and disabled GitHub intake do not
      degrade health due to missing webhook secret.
- [ ] Route tests: `/api/intake` reports GitHub mode, webhook driver state,
      polling driver state, and effective repos.
- [ ] Route tests: webhook handler rejects whitespace-only configured secrets
      without accepting the request as signed.
- [ ] Verification: `cargo test -p harness-server intake_status`.
- [ ] Verification: `cargo test -p harness-server health`.
- [ ] Verification: `cargo test -p harness-server github_webhook`.
- [ ] Verification: `cargo check -p harness-server --all-targets`.

## Rollback Plan

Revert the startup validation, intake status metadata additions, and webhook
secret normalization changes. No data migration or persisted state changes are
required.
