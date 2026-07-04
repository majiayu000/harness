# Tech Spec

## Linked Issue

GH-1473

## Product Spec

See `product.md`.

## Codebase Context

| Area | Files | Current behavior | Why relevant |
| --- | --- | --- | --- |
| Agent-driven poll | `crates/harness-core/src/config/workflow.rs:476-501`, `runtime/repo_backlog.rs:68` | poll_repo_backlog dispatched as codex_exec agent | expensive path |
| REST poller | `crates/harness-server/src/intake/github_issues.rs:306-362` | full poller implemented | exists, unregistered |
| Registration | `crates/harness-server/src/http/builders/intake.rs:90` | REST poller not wired | wiring point |
| Throttle precedent | `crates/harness-server/src/reconciliation.rs:98-132` | per-minute REST rate limiting | pattern to reuse |
| Dedupe | intake external-id path | webhook dedupe exists | invariant P3 anchor |

## Proposed Design

1. Config: `intake.discovery` = `agent` (default, unchanged) | `rest`.
2. When `rest`, register the `github_issues` poller in `builders/intake.rs`
   and disable the agent-driven `poll_repo_backlog` dispatch for those repos.
3. Reuse the reconciliation throttle pattern for REST pacing; batch cursor
   round-robins repos so late-list repos are not starved.
4. Discovered issues flow through the existing `IntakeSource` dedupe/dispatch
   (`intake/mod.rs`) exactly like webhook events.

## Product-to-Test Mapping

| Product invariant | Implementation area | Verification |
| --- | --- | --- |
| P1 zero agent spawns | discovery mode gating | test: rest sweep records no spawn |
| P2 throttled | poller pacing | unit test on throttle math |
| P3 single task per issue | external-id dedupe | test: webhook + rest same issue → 1 task |

## Alternatives Considered

- Keeping agent-only polling with bigger intervals — still pays agents to
  discover nothing; does not fix the cost class.
- GraphQL bulk queries — optimization on top; REST reuse is the smaller step.
