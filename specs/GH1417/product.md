# Product Spec

## Linked Issue

GH-1417

## User Problem

Two long-horizon operational gaps compound each other:

1. Workflow instances that reach `blocked` or sit in `awaiting_feedback` have no automated escalation. The operator dashboard can show stalled tasks on request, but nothing surfaces aged workflow instances proactively; a repo's intake can quietly stall for days behind one parked instance.
2. The runtime's event and job tables grow forever. There is no retention or pruning, so long-running deployments accumulate unbounded history — the same only-grows failure class as the per-workspace schema leak fixed by the orphan reaper, one layer up.

## Goals

- No workflow instance can stay silently parked past a configurable age; aged instances appear in an explicit stuck/dead-letter list and in error-level logs.
- Storage for terminal (finished) workflows is bounded by a configurable retention window.
- Both behaviors follow the existing sweeper pattern: background timer, config-gated, observable.

## Non-Goals

- Outbound alert transports (Slack, PagerDuty, email) — surfacing here means a queryable list plus error-level logs; push notification is a separate feature.
- Changing state-machine semantics for healthy workflows or adding new workflow states.
- Archival of pruned events to external storage (possible later phase; this is bounded deletion only).
- Retention for non-runtime tables (task history, eval runs).

## User-Visible Behavior

- A new watchdog sweeper runs on a timer. Instances in non-terminal wait states older than the threshold are listed by a new operator API endpoint (dead-letter/stuck list) and logged at error level with workflow id, definition, state, and age. Where a recoverable route exists, the watchdog can route the instance through the existing decision machinery instead of only reporting it.
- A retention sweeper prunes events and jobs belonging to workflow instances that have been terminal longer than the retention window. Active instances are never touched. Each sweep logs what it deleted.
- Both sweepers are off by default at first release, enabled by config, with documented enable guidance.

## Acceptance Criteria

- [ ] An instance stuck in `blocked`/`awaiting_feedback` beyond the threshold appears in the operator stuck list and produces an error-level log entry.
- [ ] Retention pruning deletes events/jobs only for instances terminal longer than the window; a test covers the "instance still active" negative case.
- [ ] Both sweepers are config-gated with sane defaults documented; disabled means bit-for-bit current behavior.
- [ ] On a long-running dev deployment, table growth is measurably bounded (before/after row counts recorded in the implementation PR).

## Edge Cases

- Instance transitions out of the wait state between detection and reporting: report is best-effort snapshot; no action is taken on instances that moved on.
- Terminal instance is a parent whose children are still active: prune only when the whole family is terminal, so parent-completion propagation evidence is never deleted mid-flight.
- Retention window shorter than reconciliation lookback: config validation enforces retention >= the longest consumer lookback, or warns explicitly.
- Very large backlogs on first enable: pruning works in bounded batches per tick so the first sweep cannot monopolize the database.

## Rollout Notes

Off-by-default sweeper flags mean zero behavior change on upgrade. Recommended enable order: watchdog first (read-only reporting), retention second once the stuck list is clean. Postgres and SQLite must both be covered or the limitation stated explicitly in config docs.
