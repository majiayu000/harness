# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.6.34](https://github.com/majiayu000/harness/releases/tag/harness-agents-v0.6.34) - 2026-06-12

### Added

- *(workflow)* central WORKFLOW.md config + drive Codex turns through codex exec ([#1222](https://github.com/majiayu000/harness/pull/1222))
- *(observe)* wire token usage overview KPIs
- *(security)* CapabilityToken for parallel_dispatch write isolation ([#826](https://github.com/majiayu000/harness/pull/826))
- *(671)* model tiering, quota circuit-breaker, and release-plz CI ([#778](https://github.com/majiayu000/harness/pull/778))
- *(observe)* add LLM-specific metrics (cost/turns/linter/latency) ([#738](https://github.com/majiayu000/harness/pull/738))
- *(agents)* add model and reasoning effort to Codex agent
- *(agents)* enable per-phase model selection and effort control
- *(protocol)* add steer/approval routing infrastructure for agent adapters
- restore hard tool enforcement via --allowedTools at CLI boundary
- true SSE streaming for task execution via GET /tasks/{id}/stream
- integrate ReasoningBudget per-phase model selection
- wire multi-agent dispatch and register anthropic-api adapter ([#168](https://github.com/majiayu000/harness/pull/168))
- *(agents)* stream execute_stream deltas in realtime
- add AdapterRegistry + apply Gemini review feedback ([#121](https://github.com/majiayu000/harness/pull/121))
- add CodexAdapter App Server JSON-RPC protocol ([#120](https://github.com/majiayu000/harness/pull/120))
- add ClaudeAdapter stream-json parser ([#119](https://github.com/majiayu000/harness/pull/119))
- PR review loop + agent observability fixes
- initial Harness platform — 10 Rust crates implementing OpenAI Harness Engineering

### Fixed

- *(runtime)* stop retrying Codex quota failures ([#1285](https://github.com/majiayu000/harness/pull/1285))
- *(runtime)* pass Codex approval policy via config ([#1230](https://github.com/majiayu000/harness/pull/1230))
- *(runtime)* honor codex exec transport ([#1172](https://github.com/majiayu000/harness/pull/1172))
- *(review)* make hosted bots advisory by default
- *(codex)* consume structured Codex events ([#1038](https://github.com/majiayu000/harness/pull/1038))
- *(pr-detection)* only match PRs that claim to close the issue ([#799](https://github.com/majiayu000/harness/pull/799)) ([#801](https://github.com/majiayu000/harness/pull/801))
- *(agents)* increase stream idle timeout from 30min to 60min ([#773](https://github.com/majiayu000/harness/pull/773))
- *(security)* add prompt injection barrier and field truncation for auto-fix tasks ([#616](https://github.com/majiayu000/harness/pull/616))
- *(agents)* use process groups to prevent orphaned grandchild processes
- *(agents)* place prompt immediately after -p flag in Claude CLI invocation
- *(agents)* pass prompt as positional arg to claude CLI, not via stdin
- remove --no-session-persistence to restore token usage tracking
- *(agents)* log warn when child.kill() fails in codex_adapter
- *(agents)* allow PATH-resolved cli_path in probe_no_session_persistence
- *(agents)* address PR #610 review: path validation, log level, dead UI warning
- *(agents)* add 5 s timeout to probe_no_session_persistence; fix token-usage dir error handling
- *(agents)* strip CLAUDE env vars before probing --no-session-persistence; warn on missing session dir
- *(agents)* probe CLI for --no-session-persistence support instead of unconditional flag
- *(agents)* add --no-session-persistence for background agent spawns
- *(agents)* pipe prompt via stdin instead of positional CLI argument
- align codex tool semantics and planning-gate exclusions
- address three reviewer issues in planning gate and capability profiles
- use bypassPermissions (camelCase) for --permission-mode flag
- distinguish deny-all from full-access and add bypass-permissions for restricted mode
- use char-safe truncation for stderr lines
- detect quota exhaustion and fail fast instead of looping
- use char-safe slicing for stdout_tail to prevent UTF-8 panic
- improve agent crash diagnostics with stdout capture and startup logging
- suppress false WARN for code-content lines in Codex stderr
- *(streaming)* fix two stream_timeout_secs correctness issues found in PR review
- *(streaming)* add configurable stream timeout for zombie connections
- eliminate TOCTOU in HOME-based path validation ([#489](https://github.com/majiayu000/harness/pull/489)) ([#503](https://github.com/majiayu000/harness/pull/503))
- remove --allowedTools flag that crashes Claude CLI 2.1.70 ([#483](https://github.com/majiayu000/harness/pull/483)) ([#493](https://github.com/majiayu000/harness/pull/493))
- set CARGO_TARGET_DIR per workspace to prevent build lock contention
- *(test)* reap child process in channel-closed test to prevent zombie leaks
- prevent agent process hangs by closing stdin and enabling kill_on_drop
- log mpsc send errors instead of silently ignoring them
- codex CLI 0.114.0 sandbox flag migration and config-file project registration
- make timeout stream test CI-resilient with continuous stdout output
- make codex stream cancel test CI-resilient
- add kill_on_drop and centralize strip_claude_env across all agents
- align test timeouts and sandbox default assertions
- add env_remove for CLAUDECODE/CLAUDE_CODE_ENTRYPOINT to CodexAgent ([#190](https://github.com/majiayu000/harness/pull/190))
- remove all Claude nesting detection env vars to prevent SIGTRAP
- add MessageDelta match arm and apply cargo fmt
- address OS sandbox rework feedback ([#156](https://github.com/majiayu000/harness/pull/156))
- codex cloud execution container isolation ([#150](https://github.com/majiayu000/harness/pull/150))
- log anthropic stream errors + make review bot command configurable ([#123](https://github.com/majiayu000/harness/pull/123))
- make anthropic max_tokens config-driven ([#116](https://github.com/majiayu000/harness/pull/116))
- surface mpsc send errors in claude/codex streams (FUT-27) ([#69](https://github.com/majiayu000/harness/pull/69))
- address Gemini review — registry Arc, input validation, functional style
- remove library eprintln!, generalize run_review_loop signature
- eliminate unwrap/expect — dispatch() returns Result
- enable agentic mode in ClaudeCodeAgent

### Other

- *(release)* v0.6.34 ([#1196](https://github.com/majiayu000/harness/pull/1196))
- *(agents)* cover codex turn EOF failure ([#1140](https://github.com/majiayu000/harness/pull/1140))
- Harden Codex app-server turn adapters
- Harden workflow runtime under Codex defaults
- *(harness-agents/codex)* extract tests via #[path] ([#1092](https://github.com/majiayu000/harness/pull/1092))
- Fix workflow runtime workspace and Codex recovery ([#1062](https://github.com/majiayu000/harness/pull/1062))
- Add workflow runtime activity result contracts ([#1047](https://github.com/majiayu000/harness/pull/1047))
- Add structured turn telemetry to task and event surfaces ([#915](https://github.com/majiayu000/harness/pull/915))
- Clean runtime adapter strategy and docs links ([#1019](https://github.com/majiayu000/harness/pull/1019))
- Stabilize GitHub auth and workspace defaults ([#1010](https://github.com/majiayu000/harness/pull/1010))
- Improve Codex runtime diagnostics and intake parsing ([#1004](https://github.com/majiayu000/harness/pull/1004))
- Make issue workflows the orchestration authority ([#913](https://github.com/majiayu000/harness/pull/913))
- Prevent expired capability tokens from reaching Anthropic agent calls
- split oversized files into modules (closes #625) ([#823](https://github.com/majiayu000/harness/pull/823))
- split build_app_state into domain-specific builders ([#685](https://github.com/majiayu000/harness/pull/685)) ([#746](https://github.com/majiayu000/harness/pull/746))
- unify AgentRegistry and AdapterRegistry into descriptor registry ([#687](https://github.com/majiayu000/harness/pull/687)) ([#743](https://github.com/majiayu000/harness/pull/743))
- Stabilize Codex stream lifecycle tests under scheduler jitter
- *(harness-agents)* stabilize stream drop cleanup checks
- checkpoint current harness workspace state
- Decouple agent routing and remove silent degradation ([#561](https://github.com/majiayu000/harness/pull/561))
- Merge pull request #508 from majiayu000/feat/stream-timeout-332
- replace env_vars loop with cmd.envs() in claude and codex agents
- Merge pull request #423 from majiayu000/feat/issue-193-db-serializable-trait
- fix flaky claude timeout test to mirror robust codex pattern
- Merge pull request #409 from majiayu000/feat/issue-168-dispatch-coverage
- Merge pull request #407 from majiayu000/feat/issue-166-fix-scoped-env-var-multi-key
- *(streaming)* strengthen assertions to catch output reordering and extra deltas
- *(streaming)* add direct unit tests for stream_child_output helper
- *(codex)* add focused tests for cloud setup cache and secret isolation ([#396](https://github.com/majiayu000/harness/pull/396))
- Merge pull request #361 from majiayu000/feat/fix-silent-error-discards
- Merge pull request #350 from majiayu000/feat/log-mpsc-send-errors
- increase timeout test sleep duration for CI reliability
- Merge pull request #216 from majiayu000/feat/filter-agent-stderr
- Merge pull request #189 from majiayu000/feat/codex-remove-claude-env-vars
- Merge pull request #182 from majiayu000/feat/wire-completion-callback
- *(agents)* extract shared stream_child_output helper
- prune dead modules and tighten harness-server test sandboxes
