# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

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
