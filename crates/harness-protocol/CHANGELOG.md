# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.7.1](https://github.com/majiayu000/harness/releases/tag/harness-protocol-v0.7.1) - 2026-10-21

### Added

- *(runtime)* deliver rendered prompts to remote hosts ([#2071](https://github.com/majiayu000/harness/pull/2071))
- *(eval)* add workflow execution mode ([#1938](https://github.com/majiayu000/harness/pull/1938))
- *(workflow)* retain run evidence artifacts ([#1921](https://github.com/majiayu000/harness/pull/1921))
- add context composer ([#1432](https://github.com/majiayu000/harness/pull/1432))
- *(skills)* add freshness classifier and skill/stale endpoint ([#602](https://github.com/majiayu000/harness/pull/602))
- *(protocol)* add steer/approval routing infrastructure for agent adapters
- resumable workflows with contract-driven evaluator loop ([#572](https://github.com/majiayu000/harness/pull/572))
- *(agents)* stream execute_stream deltas in realtime
- add MessageDelta notification and /ws route for streaming ([#104](https://github.com/majiayu000/harness/pull/104))
- protocol compatibility — slash-style methods + initialize handshake ([#117](https://github.com/majiayu000/harness/pull/117))
- implement issue #41 step1 initialized handshake support ([#51](https://github.com/majiayu000/harness/pull/51))
- add stdio server-push notifications pipeline (issue #41 step 2) ([#45](https://github.com/majiayu000/harness/pull/45))
- implement Phase 2 complexity routing and contract validation
- Phase 6 health report and stats for VibeGuard absorption
- Phase 5 learn feedback loop for VibeGuard absorption
- initial Harness platform — 10 Rust crates implementing OpenAI Harness Engineering

### Fixed

- *(console)* preserve operator state and decouple history polling ([#2112](https://github.com/majiayu000/harness/pull/2112))
- *(runtime)* deliver native quality gate claims ([#2099](https://github.com/majiayu000/harness/pull/2099))
- *(server)* include workflow-runtime work in operator-snapshot ([#2047](https://github.com/majiayu000/harness/pull/2047))
- *(api)* expose cached runtime input usage ([#2045](https://github.com/majiayu000/harness/pull/2045))
- *(runtime)* complete agent contract production dogfood ([#2031](https://github.com/majiayu000/harness/pull/2031))
- *(runtime)* fence remote leases with proof and expiry ([#1975](https://github.com/majiayu000/harness/pull/1975))
- *(observe)* harden event store telemetry queries ([#1852](https://github.com/majiayu000/harness/pull/1852))
- *(codex)* consume structured Codex events ([#1038](https://github.com/majiayu000/harness/pull/1038))
- add serde(default) to EvalResult::Partial.passed field
- resolve CI failures — fmt, import paths, and clippy lints
- address round-3 review issues — planning gate, checkpoint durability, fenced-block parsing
- resolve pre-existing CI compile errors in harness-core and harness-protocol
- address OS sandbox rework feedback ([#156](https://github.com/majiayu000/harness/pull/156))
- add semantic error codes instead of uniform INTERNAL_ERROR ([#91](https://github.com/majiayu000/harness/pull/91))

### Other

- Connect Console v3 to saved and runnable eval reports ([#2117](https://github.com/majiayu000/harness/pull/2117))
- Connect Console v3 local quotas and rule operations ([#2116](https://github.com/majiayu000/harness/pull/2116))
- Restore Console v2 dashboard design with live Harness data ([#2111](https://github.com/majiayu000/harness/pull/2111))
- Preserve Cursor runtime and review baseline ([#2057](https://github.com/majiayu000/harness/pull/2057))
- *(cli)* decouple reconcile from server ([#2034](https://github.com/majiayu000/harness/pull/2034))
- *(protocol)* centralize CLI JSON-RPC boundary ([#1854](https://github.com/majiayu000/harness/pull/1854))
- *(protocol)* own context preview request DTOs ([#1723](https://github.com/majiayu000/harness/pull/1723))
- *(server)* remove legacy task layer ([#1706](https://github.com/majiayu000/harness/pull/1706))
- *(server)* remove legacy thread lifecycle RPCs ([#1703](https://github.com/majiayu000/harness/pull/1703))
- Unify build warning policy and dev debug profile ([#1542](https://github.com/majiayu000/harness/pull/1542))
- release v0.6.34
- *(release)* v0.6.34 ([#1196](https://github.com/majiayu000/harness/pull/1196))
- Add structured turn telemetry to task and event surfaces ([#915](https://github.com/majiayu000/harness/pull/915))
- Merge pull request #600 from majiayu000/feat/issue-591-claude-adapter-steer-approval
- use let-else in partial-without-passed test for clarity
- use pattern matching in partial-without-passed test
- Merge pull request #578 from majiayu000/feat/573-checkpoint-recovery-integration-tests
- checkpoint current harness workspace state
- document HTTP/JSON-RPC transport split (closes #337)
- Merge branch 'feat/phase7-cross-review' into feat/vibeguard-integration
- add unit tests for 8 crates (56 tests total)

### Removed

- *(breaking)* remove Harness-owned thread/turn lifecycle RPC methods and the
  unwritten `context/manifest/get` request surface.

## [0.6.34](https://github.com/majiayu000/harness/releases/tag/harness-protocol-v0.6.34) - 2026-06-29

### Added

- *(skills)* add freshness classifier and skill/stale endpoint ([#602](https://github.com/majiayu000/harness/pull/602))
- *(protocol)* add steer/approval routing infrastructure for agent adapters
- resumable workflows with contract-driven evaluator loop ([#572](https://github.com/majiayu000/harness/pull/572))
- *(agents)* stream execute_stream deltas in realtime
- add MessageDelta notification and /ws route for streaming ([#104](https://github.com/majiayu000/harness/pull/104))
- protocol compatibility — slash-style methods + initialize handshake ([#117](https://github.com/majiayu000/harness/pull/117))
- implement issue #41 step1 initialized handshake support ([#51](https://github.com/majiayu000/harness/pull/51))
- add stdio server-push notifications pipeline (issue #41 step 2) ([#45](https://github.com/majiayu000/harness/pull/45))
- implement Phase 2 complexity routing and contract validation
- Phase 6 health report and stats for VibeGuard absorption
- Phase 5 learn feedback loop for VibeGuard absorption
- initial Harness platform — 10 Rust crates implementing OpenAI Harness Engineering

### Fixed

- *(codex)* consume structured Codex events ([#1038](https://github.com/majiayu000/harness/pull/1038))
- add serde(default) to EvalResult::Partial.passed field
- resolve CI failures — fmt, import paths, and clippy lints
- address round-3 review issues — planning gate, checkpoint durability, fenced-block parsing
- resolve pre-existing CI compile errors in harness-core and harness-protocol
- address OS sandbox rework feedback ([#156](https://github.com/majiayu000/harness/pull/156))
- add semantic error codes instead of uniform INTERNAL_ERROR ([#91](https://github.com/majiayu000/harness/pull/91))

### Other

- *(release)* v0.6.34 ([#1196](https://github.com/majiayu000/harness/pull/1196))
- Add structured turn telemetry to task and event surfaces ([#915](https://github.com/majiayu000/harness/pull/915))
- Merge pull request #600 from majiayu000/feat/issue-591-claude-adapter-steer-approval
- use let-else in partial-without-passed test for clarity
- use pattern matching in partial-without-passed test
- Merge pull request #578 from majiayu000/feat/573-checkpoint-recovery-integration-tests
- checkpoint current harness workspace state
- document HTTP/JSON-RPC transport split (closes #337)
- Merge branch 'feat/phase7-cross-review' into feat/vibeguard-integration
- add unit tests for 8 crates (56 tests total)
