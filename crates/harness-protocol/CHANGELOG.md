# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.6.34](https://github.com/majiayu000/harness/releases/tag/harness-protocol-v0.6.34) - 2026-06-24

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
