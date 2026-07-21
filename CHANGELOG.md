# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed

- **breaking:** removed the Harness-owned JSON-RPC methods `thread/start`,
  `thread/resume`, `thread/fork`, `thread/list`, `thread/delete`,
  `thread/compact`, `turn/start`, `turn/steer`, `turn/cancel`, `turn/status`,
  `turn/respond_approval`, and the unwritten `context/manifest/get` endpoint.
  Use workflow-runtime HTTP submissions for durable work; live approval
  responses now use
  `POST /api/workflows/runtime/turns/{turn_id}/approvals/{request_id}`. The
  TypeScript and Python SDKs have migrated to these HTTP surfaces.
- **breaking:** removed the legacy `/tasks` compatibility routes. Submit and
  list work through `POST` and `GET /api/workflows/runtime/submissions`; use
  `/api/workflows/runtime/submissions/{id}` for detail and append `/stream`,
  `/artifacts`, `/prompts`, or `/proof` for the corresponding read surface.
  Cancel and merge now use `POST /api/workflows/runtime/cancel` and
  `POST /api/workflows/runtime/merge` with `workflow_id`. There is no direct
  replacement for `POST /tasks/batch`; callers must submit each request to the
  runtime endpoint. Runtime hosts must claim
  `/api/runtime-hosts/{host_id}/runtime-jobs/claim`; the legacy
  `/api/runtime-hosts/{host_id}/tasks/claim` route has been removed.
- **migration:** Phase 1 data was archived before removal as
  `archives/phase1-20260716T092438Z`. The archive is operator-owned and may
  contain sensitive prompt or repository data. Inspect `table_counts.tsv` and
  `phase1-data.list`, then restore `phase1-data.dump` only into an empty scratch
  database with
  `pg_restore --exit-on-error --no-owner --no-acl --dbname "$SCRATCH_DATABASE_URL" phase1-data.dump`.
  Never restore it into a live Harness database and do not pass `--clean`.
- **maintenance:** the GH-1434 Phase 1 implementation series (#1663, #1701,
  #1702, #1703, and #1706) removed 34,156 lines and added 11,246 lines, for a
  net deletion of 22,910 lines.
- **security:** HTTP API authentication now fails closed when neither
  `api_token` nor `HARNESS_API_TOKEN` is configured. Operators must set a token
  or explicitly opt in to tokenless local development with
  `allow_unauthenticated = true`.

## [0.6.34] - 2026-05-30

First release with a tracked changelog. This is a workflow-runtime reliability
patch addressing a batch of correctness issues found in a design audit of the
workflow runtime (dispatcher / worker / reducer / validator / store / sweepers).

### Fixed

- **runtime:** PR-feedback active-command detection now treats `dispatching`
  commands as active, closing a window where a claimed-but-not-yet-dispatched
  command could be missed and a duplicate sweep/inspect job enqueued
  (#1178, #1185).
- **runtime:** `prompt_task` no longer completes as `done` without validation
  evidence, bringing it in line with the issue-implementation and quality-gate
  completion contracts (#1179, #1186).
- **runtime:** disabling the runtime worker now also short-circuits already
  queued jobs, while builtin lifecycle activities (e.g. `mark_bound_issue_done`)
  keep flowing so workflows are not stranded; transient preflight errors defer
  to the normal execute path instead of permanently failing the job
  (#1181, #1187).
- **runtime:** reducer/validator now validate replayed events at the event's
  own timestamp instead of `Utc::now()`, making event replay deterministic for
  lease-expiry checks (#1182, #1188).
- **runtime:** `repo_backlog` workflows stuck in `blocked` are now recovered by
  the stale-workflow recovery tick instead of stranding a repo's intake; the
  root-cause stateless redesign remains tracked separately (#1180, #1193).
- **runtime:** PR-feedback child lookup is scoped by `parent_workflow_id`
  instead of scanning every PR-feedback instance across all projects, and the
  no-actionable-feedback suppression honors fresh PR activity (#1183, #1194).

### Changed

- Added crate metadata (`description`, `license`, `repository`) and internal
  dependency versions across the workspace so the crates can be published.

[0.6.34]: https://github.com/majiayu000/harness/releases/tag/v0.6.34
