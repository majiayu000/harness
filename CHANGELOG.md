# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.7.0] - 2026-10-20

This release consolidates the workflow-runtime, agent integration, evaluation,
and operator tooling changes since v0.6.34. It includes breaking API changes;
review the upgrade notes below before updating a running server.

### Added

- **workflows:** declarative workflow definitions, intake bindings, candidate
  selection and promotion, and durable replay transcripts (#1523–#1526, #1627,
  #1629, #1630, #1688, #1710).
- **agents:** OpenCode execution and per-turn ACP backends, repository-observed
  agent stack inventory, and pinned agent-contract activities (#1810, #1882,
  #2020, #2025, #2028).
- **evaluation:** manifest-driven runs, report/diff commands, case-level
  regression reports, attestations, and a supervised Docker task host with
  retained candidates and native quality gates (#1918, #1923, #1930, #2061,
  #2102, #2115). Live credential-backed runs and agent-upgrade trials have
  explicit entry points; production baselines still require operator evidence.
- **operations:** an operator monitor cockpit, agent usage views, runtime
  trajectory spans, alerting, and budget enforcement controls (#1228, #1328,
  #1497, #1500, #1592, #1893, #1903–#1906).

### Fixed

- **security:** update rustls to 0.23.46, resolving RUSTSEC-2026-0346 through
  RUSTSEC-2026-0349 without adding advisory exceptions.
- **runtime:** detect issue dependency deadlocks, dead-letter expired activity
  completions, and preserve trusted completion evidence (#1887, #1901, #1952).
- **recovery:** stop retrying unchanged readiness gates and preserve operator
  cancellation when recovering issues (#2159, #2161).
- **accounting:** stop enforced turns when stream usage accounting fails
  instead of letting them continue without reliable usage evidence (#2160).

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

### Upgrade notes

- Update clients and SDK integrations to the workflow-runtime HTTP endpoints
  listed above before replacing a v0.6.x server.
- Review the current configuration example and configure API authentication.
  Fleet/server operation requires PostgreSQL 14+; one-shot CLI execution does
  not require the server or PostgreSQL.
- Distribution remains source-based through GitHub releases. This release
  does not publish crates, SDK packages, or prebuilt binaries.
- Open roadmap items, including live evaluation baselines, semantic retrieval,
  and production spend verification, remain follow-up work rather than release
  acceptance claims (#1768, #1769, #1770).

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

[Unreleased]: https://github.com/majiayu000/harness/compare/v0.7.0...HEAD
[0.7.0]: https://github.com/majiayu000/harness/compare/v0.6.34...v0.7.0
[0.6.34]: https://github.com/majiayu000/harness/releases/tag/v0.6.34
