# GH1582 Product Spec: External Alerting Channel for Task/Workflow Failures

GitHub issue: `#1582`

## Goals

- Push critical failure signals out of the Harness process: task failures
  after retry exhaustion, workflow blocked/failed transitions,
  circuit-breaker opens, reconciliation anomalies, and `ready_to_merge`
  aging must reach an external channel without a human polling the
  dashboard.
- Provide one generic outbound webhook contract (JSON payload, retry with
  backoff, delivery audit trail) plus Slack and Feishu formatters on top.
- Detect the case internal observability cannot cover: the server itself
  is down, via an external heartbeat / dead-man-switch ping.
- Fix the silent-drop path in `notify.rs` so dropped in-process
  notifications are counted and surfaced at error level.
- Feed GH-1584 (auto-recovery escalation), which will consume these alert
  event classes as its escalation input.

## Non-Goals

- No PagerDuty integration, on-call scheduling, or paging escalation
  chains — the generic webhook covers future integrations.
- No email or SMS delivery.
- No inbound alert acknowledgement flow; delivery is one-way push.
- Feature is OFF by default; nothing changes for unconfigured deployments.

## Users

- Operators running Harness unattended who currently learn about failures
  only from the dashboard, the `harness:stuck` GitHub label, or
  intake-scoped Feishu replies.
- GH-1584 auto-recovery escalation, which consumes the alert event stream.

## Behavior Invariants

- `B-001` With no `[alerting]` config section, the feature is inert: no
  outbound HTTP requests, no background dispatcher or heartbeat task is
  spawned, and existing config files parse unchanged.
- `B-002` Enabling alerting with an invalid channel definition (missing or
  malformed URL, unknown adapter kind) fails config validation at startup
  with an error; there is no silent partial activation.
- `B-003` Only configured alert event classes fire. The class set is a
  closed enum: `task_failure_exhausted`, `workflow_blocked`,
  `workflow_failed`, `circuit_breaker_open`, `reconciliation_anomaly`,
  `ready_to_merge_aging`, `notify_channel_drop`. Unlisted event kinds never
  produce outbound traffic.
- `B-004` A task failure alert fires exactly once per terminal exhaustion
  (retry budget spent or stuck escalation), not once per retry attempt.
- `B-005` The webhook payload is a stable JSON contract carrying
  `schema_version`, `event_class`, `severity`, `timestamp`, `project`,
  `subject` (task/workflow/PR identifiers), `message`, and `dedup_key`.
  Fields are additive-only across versions.
- `B-006` A failed delivery is retried with exponential backoff up to
  `max_attempts` (default 3). After exhaustion the delivery is recorded as
  failed; it is never re-enqueued implicitly.
- `B-007` Every delivery attempt (success or failure) records an audit
  event in the observe event store: channel name, attempt number, HTTP
  status or error string, and duration. The audit trail distinguishes
  first attempts from retries.
- `B-008` A delivery that exhausted retries is surfaced at error level and
  is queryable as failed; degraded delivery never looks like success on
  the dashboard or in the event store.
- `B-009` No alert loops: failures inside the alerting pipeline itself
  (delivery errors, adapter formatting errors, queue drops) never enqueue
  new outbound alerts. They surface only as audit events and error-level
  logs.
- `B-010` Alerts sharing a `dedup_key` within the cooldown window
  (default 300s) are suppressed; each suppression increments a counter
  recorded in the audit trail, so suppression is observable, not silent.
- `B-011` The dispatcher queue is bounded and never blocks producers.
  When the queue is full, the alert is dropped with a counter increment
  and an error-level structured log; task/workflow execution paths are
  never stalled by alerting.
- `B-012` `notify.rs` websocket-push drops increment the existing counter
  AND emit an error-level structured event (rate-limited), replacing the
  current warn-only logging. When alerting is enabled, sustained drops
  map to the `notify_channel_drop` alert class.
- `B-013` Adapter formatting failures (Slack/Feishu payload construction)
  are recorded as failed deliveries in the audit trail at error level;
  there is no silent fallback that masquerades as a formatted delivery.
- `B-014` The Feishu adapter reuses the existing tenant-access-token
  plumbing; a token-fetch failure is a delivery failure subject to the
  same retry/backoff and audit rules as any HTTP failure.
- `B-015` Webhook URLs, tokens, and secrets never appear in audit events,
  payloads, or logs; audit records reference channels by configured name
  only.
- `B-016` When heartbeat is enabled, a ping fires every
  `heartbeat.interval_secs`; a missed or failed ping logs at warn level
  but never generates a self-alert (server-down detection is the external
  dead-man service's job). Heartbeat is independently OFF by default.
- `B-017` Channels deliver independently: a down or slow channel does not
  block or fail delivery to other configured channels for the same alert.
- `B-018` On graceful shutdown, queued alerts get a bounded flush window;
  alerts still undelivered at timeout are recorded as failed in the audit
  trail — never silently lost.
- `B-019` `ready_to_merge` aging alerts respect the existing
  reconciliation TTL semantics: at most one alert per aging PR per
  `ready_to_merge_alert_ttl_secs` window.
- `B-020` Circuit-breaker open and reconciliation-anomaly alerts carry the
  affected entity references (profile name, workflow/task/PR ids) so a
  receiver can act without querying the server.

## Boundary Checklist

| Category | Coverage |
|---|---|
| Empty input | covered: B-001 (no config = inert), B-002 (empty/invalid channel rejected) |
| Failure paths | covered: B-006, B-008, B-013, B-014 |
| Authorization | covered: B-015 (secret redaction); inbound auth N/A — outbound-only feature, no new inbound endpoints |
| Concurrency | covered: B-011 (non-blocking bounded queue), B-017 (independent channels) |
| Retry + idempotency | covered: B-004 (once per terminal failure), B-006 (bounded backoff), B-010 (dedup key + cooldown) |
| Illegal transitions | covered: B-003 (closed event-class enum; unlisted kinds never dispatch) |
| Compatibility | covered: B-001, B-005 (additive-only payload schema), B-019 (reuses existing TTL semantics) |
| Degradation / fallback | covered: B-008, B-009 (alerting failures never loop into alerts), B-012, B-016 |
| Evidence / audit | covered: B-007 (per-attempt audit events), B-010 (suppression counters), B-018 (shutdown losses recorded) |
| Cancellation / partial | covered: B-018 (bounded flush on shutdown, remainder marked failed) |

Cross-product boundaries called out explicitly: a delivery that failed,
was retried, and exhausted must show one coherent audit chain (B-006 +
B-007 + B-008); an alert channel being down while the system is alerting
about failures must terminate in the audit trail, not in a feedback loop
(B-009 + B-011).
