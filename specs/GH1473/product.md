# Product Spec

## Linked Issue

GH-1473

## User Problem

Operators that cannot receive GitHub webhooks still need poll-mode backlog
discovery, but polling must not spend agent tokens just to list open issues.
The current server has a direct GitHub REST issue poller and direct issue
dispatch plumbing, yet poll-mode behavior is not exposed as an explicit
REST-only discovery contract. It is also difficult to tell from configuration
whether polling will only collect remote issue facts or whether richer
agent-driven backlog analysis is expected.

## Goals

- Let operators configure GitHub poll mode to use REST-only issue discovery.
- In REST-only discovery mode, a poll sweep must not spawn agent processes just
  to discover open GitHub issues.
- Keep discovered issues on the existing intake dedupe, coverage, and dispatch
  path so webhook and poll discovery converge before task execution.
- Keep agent-driven backlog analysis available as an explicit opt-in where
  richer issue interpretation is desired.
- Make REST polling rate-limit aware so repeated sweeps do not hot-loop on
  GitHub `429` or secondary-rate-limit responses.
- Surface enough status/logging for operators to see whether polling is active,
  throttled, disabled, or degraded.

## Non-Goals

- Removing webhook or hybrid intake modes.
- Removing the direct GitHub issue poller, coverage gate, or direct dispatch
  path.
- Changing how implementation tasks execute after an issue has been accepted
  for work.
- Changing auto-merge policy, review gates, or PR feedback handling.
- Adding a new GitHub authentication mechanism beyond the existing server token
  resolution path.

## User-Visible Behavior

When GitHub intake is enabled in `poll` or `hybrid` mode and configured for
REST-only discovery, Harness periodically lists configured repository issues
through GitHub REST, filters already covered or dispatched issues, and submits
uncovered issues through the same server-side intake path used by webhook
events. The discovery sweep itself does not run Codex, Claude, or another
agent.

When GitHub returns a rate-limit response, Harness records the throttle signal,
skips or delays all repository polling that shares the throttled token until the
safe retry time, and does not spin or enqueue duplicate work.

If workflow runtime persistence is unavailable, REST polling is disabled or
suspended before pollers start. Harness must not repeatedly rediscover issues
that it cannot persist or dispatch.

Operators that want agent-driven backlog analysis can select that discovery
driver explicitly. Existing webhook-only deployments remain webhook-only and do
not start a poller.

## Acceptance Criteria

- [ ] GitHub intake config has an explicit discovery driver for poll-capable
      modes, including REST-only discovery and agent-driven discovery.
- [ ] REST-only discovery registers direct GitHub REST pollers in `poll` and
      `hybrid` modes and does not require an agent process for a sweep that only
      discovers remote issues.
- [ ] Webhook-only mode still does not register GitHub polling sources.
- [ ] Issues discovered by REST-only polling enter the existing GitHub intake
      coverage, dedupe, project workflow, and direct dispatch path.
- [ ] REST-only polling fails closed or remains suspended when workflow runtime
      persistence is unavailable, before any GitHub issue listing calls are
      made.
- [ ] REST polling handles GitHub rate-limit responses using response headers
      such as `Retry-After` and/or `X-RateLimit-Reset`, and avoids immediate
      retry loops across all repositories sharing the throttled token.
- [ ] Intake status or logs identify the active GitHub discovery driver and
      whether polling is active or throttled.
- [ ] Tests cover config parsing/defaults, poller registration by mode/driver,
      zero-agent discovery behavior, same-path dispatch for discovered issues,
      and rate-limit throttling.

## Edge Cases

- `poll` mode with REST-only discovery and no configured repositories should
  start no pollers and should log a clear no-source message.
- `webhook` mode must remain poller-free even if a discovery driver is set.
- `hybrid` mode should keep webhook acceptance enabled while REST polling acts
  as a backstop.
- A token-level GitHub rate limit observed by one repository must suspend other
  repositories that share the same token until the safe retry time.
- Paginated REST responses that hit the existing page limit or repeat a `Link`
  URL must not evict dispatched issue state from incomplete data.
- If workflow runtime persistence is unavailable, REST pollers should not be
  registered or should remain suspended before making GitHub API calls.

## Rollout Notes

Use `Refs #1473` for the spec PR. The implementation PR should use
`Closes #1473` after the config, poller registration, rate-limit handling,
status/logging, docs, and tests land.
