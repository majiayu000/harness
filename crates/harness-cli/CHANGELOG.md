# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.6.34](https://github.com/majiayu000/harness/compare/harness-cli-v0.0.9...harness-cli-v0.6.34) - 2026-06-07

### Added

- *(workflow)* central WORKFLOW.md config + drive Codex turns through codex exec ([#1222](https://github.com/majiayu000/harness/pull/1222))
- *(db)* automatic orphan path-schema reaper (storage RFC Phase 3b) ([#1216](https://github.com/majiayu000/harness/pull/1216))
- *(observe)* persist serve runtime logs by default ([#1045](https://github.com/majiayu000/harness/pull/1045))
- *(reconciliation)* periodic GitHub<->harness state reconciliation ([#951](https://github.com/majiayu000/harness/pull/951)) ([#958](https://github.com/majiayu000/harness/pull/958))
- *(config)* auto-discover config file from XDG/OS-standard paths ([#740](https://github.com/majiayu000/harness/pull/740))
- *(agents)* enable per-phase model selection and effort control
- *(config)* env var overrides for ServerConfig ([#721](https://github.com/majiayu000/harness/pull/721))
- transient retry and PR-aware recovery on server restart
- add triage → plan → implement pipeline with role-based prompts
- add PromptParts struct, refactor implement_from_issue for cache-ready prompt structure
- periodic review system with structured output and GitHub issue creation
- add version command to display current version
- add --project CLI flag for multi-project server startup
- migrate EventStore from JSONL to SQLite
- integrate ReasoningBudget per-phase model selection
- add auto-fix pattern support to rule engine ([#247](https://github.com/majiayu000/harness/pull/247))
- per-project config via .harness/config.toml
- wire multi-agent dispatch and register anthropic-api adapter ([#168](https://github.com/majiayu000/harness/pull/168))
- *(cli)* add mcp-server mode with harness tools
- PR review loop + agent observability fixes
- wire GC CLI commands to GcAgent + EventStore + SignalDetector
- initial Harness platform — 10 Rust crates implementing OpenAI Harness Engineering

### Fixed

- *(db)* reap directory-owned path schemas ([#1218](https://github.com/majiayu000/harness/pull/1218))
- *(db)* register path-derived Postgres schemas ([#1213](https://github.com/majiayu000/harness/pull/1213))
- *(review)* make hosted bots advisory by default
- use task project root during reconciliation ([#1007](https://github.com/majiayu000/harness/pull/1007))
- preserve startup project metadata in registry ([#704](https://github.com/majiayu000/harness/pull/704))
- *(agents)* probe CLI for --no-session-persistence support instead of unconditional flag
- resolve real repo slug in pr loop when pr_url is absent
- resolve {owner}/{repo} placeholders and retry transient API errors
- use local timezone in tracing log timestamps
- *(gc)* validate artifact target_path within project root before writing
- codex CLI 0.114.0 sandbox flag migration and config-file project registration
- validate duplicate/reserved --project names, empty paths, and load guards for startup projects
- register --project entries in ProjectRegistry and validate --default-project name
- reject non-danger sandbox mode on macOS to prevent SIGTRAP
- address Gemini code review feedback
- revert SandboxMode default to WorkspaceWrite and fix clippy lints
- change default sandbox_mode to DangerFullAccess
- address OS sandbox rework feedback ([#156](https://github.com/majiayu000/harness/pull/156))
- rework OpenTelemetry export with async-safe transport checks ([#149](https://github.com/majiayu000/harness/pull/149))
- codex cloud execution container isolation ([#150](https://github.com/majiayu000/harness/pull/150))
- log anthropic stream errors + make review bot command configurable ([#123](https://github.com/majiayu000/harness/pull/123))
- pass project_root to gc_adopt_task_request instead of None ([#80](https://github.com/majiayu000/harness/pull/80))
- *(cli)* remove exec cwd unwrap panic path ([#71](https://github.com/majiayu000/harness/pull/71))
- use configured project root for GC scans (FUT-20)
- wire rule violations through observability pipeline ([#52](https://github.com/majiayu000/harness/pull/52))
- always trigger /gemini review after pushing fixes ([#31](https://github.com/majiayu000/harness/pull/31))
- always trigger /gemini review after pushing fixes ([#28](https://github.com/majiayu000/harness/pull/28))
- update pr.rs review_prompt call to match new 4-param signature
- address Gemini review — registry Arc, input validation, functional style
- robust PR number parsing and English-only CLI messages
- remove library eprintln!, generalize run_review_loop signature
- address Gemini review — LGTM check, DRY agent creation, clap flatten
- address review comments — propagate current_dir error and robust CSV parsing
- wire HTTP route handler to router::handle_request via axum State ([#6](https://github.com/majiayu000/harness/pull/6))

### Other

- *(release)* v0.6.34 ([#1196](https://github.com/majiayu000/harness/pull/1196))
- *(runtime)* paginate workflow runtime tree
- Harden Codex app-server turn adapters
- Harden workflow runtime contracts and status diagnostics ([#1115](https://github.com/majiayu000/harness/pull/1115))
- *(cli)* cover project flag parser edge cases
- Repair workflow runtime PR recovery
- Clean runtime adapter strategy and docs links ([#1019](https://github.com/majiayu000/harness/pull/1019))
- Fix task failure accounting and Postgres pool config ([#1018](https://github.com/majiayu000/harness/pull/1018))
- Stabilize GitHub auth and workspace defaults ([#1010](https://github.com/majiayu000/harness/pull/1010))
- Fix audit findings ([#981](https://github.com/majiayu000/harness/pull/981))
- Separate background review load from issue intake capacity
- unify AgentRegistry and AdapterRegistry into descriptor registry ([#687](https://github.com/majiayu000/harness/pull/687)) ([#743](https://github.com/majiayu000/harness/pull/743))
- checkpoint current harness workspace state
- Decouple agent routing and remove silent degradation ([#561](https://github.com/majiayu000/harness/pull/561))
- Merge branch 'main' into feat/stream-timeout-332
- apply cargo fmt
- Merge pull request #318 from majiayu000/fix/codex-sandbox-and-config-projects
- rename SignalThresholds to SignalThresholdsConfig
- Merge pull request #216 from majiayu000/feat/filter-agent-stderr
- Merge pull request #205 from majiayu000/feat/split-oversized-files
- split oversized files into domain-specific modules
- split commands.rs into domain-specific modules
- add CLI argument parsing tests (serve, exec, gc, pr, global config)
- implement hardened CI/CD harness GitHub Action (r2) ([#151](https://github.com/majiayu000/harness/pull/151))
- Rework Starlark execpolicy engine (r3) ([#148](https://github.com/majiayu000/harness/pull/148))
- Apply FUT-15 runtime config wiring on top of latest main ([#56](https://github.com/majiayu000/harness/pull/56))
- extract shared prompts module to harness-core
- use Arc import instead of fully qualified path
