# Tech Spec

## Linked Issue

GH-1473

## Product Spec

See `specs/GH1473/product.md`.

## Current System

| Area | Files | Current behavior | Why relevant |
| --- | --- | --- | --- |
| GitHub intake config | `crates/harness-core/src/config/intake.rs` | `GitHubIntakeConfig` has `enabled`, `mode`, repo fields, polling interval, planner fields, and retry backoff fields. `IntakeMode::poller_enabled()` returns true for `poll` and `hybrid`. | This is where an explicit discovery driver should live so operators can choose REST-only discovery versus agent-driven analysis. |
| Direct REST poller | `crates/harness-server/src/intake/github_issues.rs` | `GitHubIssuesPoller` lists GitHub issues with REST pagination, filters dispatched issues, persists dispatched state, and emits `IncomingIssue` values. Non-success HTTP responses currently error without throttle state. | This is the REST-only discovery mechanism and the rate-limit handling target. |
| Intake builder | `crates/harness-server/src/http/builders/intake.rs` | `build_intake` registers `GitHubIssuesPoller` instances only when GitHub polling is configured and `registry.workflow_runtime_store.is_some()`. `github_runtime_polling_config_requires_workflow_runtime_store` protects that gate. | The implementation must make registration depend on the new discovery driver and keep dispatch safety when runtime persistence is unavailable. |
| Orchestrator and coverage gate | `crates/harness-server/src/intake/mod.rs`, `crates/harness-server/src/intake/github_coverage_gate.rs` | Poll ticks group issues by repo, record poll workflow state, record remote facts, skip covered issues, and pass uncovered issues to direct dispatch. | REST-discovered issues should keep using this path so webhook and poll intake converge before work execution. |
| Direct dispatch | `crates/harness-server/src/intake/direct_dispatch.rs` | Uncovered GitHub issues are submitted to workflow runtime as deterministic issue handles; this path requires `workflow_runtime_store`. | REST discovery must not spawn agents, but accepted issues still need runtime persistence before actual agent execution. |
| Intake status | `crates/harness-server/src/http/github_intake_status.rs`, `crates/harness-server/src/http/tests/intake_auth_list_tests.rs` | Status reports GitHub mode, webhook/polling driver availability, configured repos, and active pollers. | Operators need to see the selected discovery driver and throttled/degraded state. |
| Docs/examples | `config/default.toml.example`, `docs/usage-guide.md` | Existing docs show `[intake.github]` and auto-merge, but not a poll discovery driver. | Operators need a documented config knob and migration expectations. |

## Proposed Design

1. Add an explicit GitHub poll discovery driver.
   - Add a serde enum such as `GitHubDiscoveryDriver` or
     `GitHubPollDiscoveryDriver` with stable snake_case values:
     `direct_rest` and `agent`.
   - Add the field to `GitHubIntakeConfig` with a compatibility-preserving
     default. If current server-side direct polling is the intended default,
     keep `direct_rest` as default and document it; otherwise default to
     `agent` and make `direct_rest` the opt-in zero-agent path.
   - Keep `IntakeMode` semantics unchanged: `webhook` disables polling;
     `poll` and `hybrid` can use the selected discovery driver.
2. Register REST pollers only for REST-capable polling.
   - Replace `github_runtime_polling_config` with a helper that considers both
     `mode.poller_enabled()` and the discovery driver.
   - In `direct_rest`, register `GitHubIssuesPoller` for every effective repo.
   - Keep webhook-only mode returning no pollers.
   - If runtime persistence is unavailable, fail closed during registration:
     do not register or start REST pollers, and expose/log a degraded reason.
     Do not allow discovery to run when dispatch cannot persist issue handles,
     because undispatched issues would be rediscovered on every tick.
3. Preserve same-path dedupe and dispatch.
   - Continue using `IntakeOrchestrator::poll_tick`,
     `github_coverage_gate::record_issue_remote_fact_snapshot`,
     `check_github_issue_coverage`, and
     `direct_dispatch::run_direct_issue_dispatch` for REST-discovered GitHub
     issues.
   - Do not add a parallel dispatch path for REST-only discovery.
   - Keep per-repo completion callbacks keyed by `github:{owner/repo}` so
     dispatched state is updated by the same poller instance.
4. Add rate-limit-aware REST throttling.
   - Extend `fetch_github_issue_page` or the `GitHubIssuesPoller` state with a
     typed throttle result that recognizes GitHub rate-limit status codes and
     headers:
     - `Retry-After` for relative retry delay.
     - `X-RateLimit-Reset` for epoch-second reset time.
   - Store throttle state at the shared-token level used by the configured
     server GitHub token. A rate-limit signal from any repo should pause REST
     calls for every poller sharing that token until the retry time.
   - Log throttling with repo, status, and retry time. Treat malformed throttle
     headers as a bounded backoff instead of immediate retry.
5. Expose operator status and docs.
   - Include the discovery driver in GitHub intake status JSON.
   - If a repo is throttled, expose or log enough data to distinguish
     "configured but waiting for GitHub rate limit" from "poller disabled".
   - Update `config/default.toml.example` and `docs/usage-guide.md` with the
     new driver values and the zero-agent discovery contract.

## Data Flow

`HarnessConfig` loads `[intake.github]` with `mode` and the new discovery
driver. During HTTP startup, `build_intake` evaluates the config. In
`direct_rest` for `poll` or `hybrid`, it registers one `GitHubIssuesPoller` per
effective repo and passes those exact `Arc<dyn IntakeSource>` instances to both
the orchestrator and completion callback.

At each poll tick, the orchestrator or poller checks shared-token throttle
state. If not throttled, it lists GitHub issues through REST, follows
pagination up to the existing page limit, and returns new `IncomingIssue`
values. The orchestrator records poll state, writes remote facts to runtime
storage, checks coverage, and dispatches uncovered issues through the existing
direct issue submission.

If workflow runtime storage is unavailable at startup, REST-only pollers are not
registered and no GitHub listing calls are made. This prevents a repeated
discover-without-dispatch loop.

Actual issue execution still happens after runtime dispatch and may use agents.
The zero-agent guarantee applies to discovery sweeps before uncovered work is
accepted for execution.

## Alternatives Considered

- Reuse `IntakeMode::Poll` to mean REST-only. Rejected because `poll` already
  means "polling is enabled"; it does not describe which discovery driver runs.
- Remove agent-driven backlog analysis. Rejected because some operators may
  still want richer analysis before issue dispatch.
- Add a second REST dispatch path. Rejected because it would bypass the current
  coverage gate, project workflow records, completion callback, and direct
  dispatch invariants.
- Ignore rate limits and rely on the outer poll interval. Rejected because a
  short poll interval can hot-loop on rate-limited repositories.

## Risks

- Security: GitHub token handling must continue to use existing server token
  resolution. Do not log token values or authorization headers.
- Compatibility: Changing the default discovery driver can surprise existing
  operators. Choose and document the default explicitly, and add config parsing
  tests.
- Performance: Polling many repos can still consume GitHub REST quota. Mitigate
  with shared token-level throttle state, existing pagination bounds, and clear
  logs.
- Maintenance: Duplicating dispatch logic would create drift. Keep REST-only
  discovery on the existing orchestrator and direct dispatch path.
- Reliability: If workflow runtime storage is unavailable, discovery without
  dispatch can hot-loop and burn GitHub quota. Fail closed before registering
  REST pollers and surface a degraded status.

## Test Plan

- [ ] `cargo test -p harness-core config::intake`
- [ ] `cargo test -p harness-server http::builders::intake`
- [ ] `cargo test -p harness-server intake::github_issues`
- [ ] `cargo test -p harness-server http::tests::intake_auth_list_tests`
- [ ] `cargo check -p harness-server --all-targets`
- [ ] `python3 checks/check_workflow.py --repo . --spec-dir specs/GH1473`
- [ ] `python3 checks/check_workflow.py --repo .`
- [ ] `cargo fmt --all -- --check`
- [ ] `cargo clippy --workspace --all-targets -- -D warnings`

## Rollback Plan

Set the new discovery driver back to the compatibility default or switch GitHub
intake to `mode = "webhook"` to stop polling. If the implementation must be
removed, revert the implementation commit; no data migration is required beyond
removing the new config field from operator config.
