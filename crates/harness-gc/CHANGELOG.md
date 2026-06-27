# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.6.34](https://github.com/majiayu000/harness/releases/tag/harness-gc-v0.6.34) - 2026-06-27

### Added

- GC incremental scanning with checkpoint and configurable tool whitelist
- add external signal ingestion for CI failures and review feedback
- add GC draft expiration and cleanup
- add incremental scanning with checkpoint to GC
- initial Harness platform — 10 Rust crates implementing OpenAI Harness Engineering

### Fixed

- *(gc)* guard adopt() and auto_adopt_matching() against non-Pending drafts ([#821](https://github.com/majiayu000/harness/pull/821))
- *(learn)* decouple grading window from gc_agent event list ([#810](https://github.com/majiayu000/harness/pull/810)) ([#813](https://github.com/majiayu000/harness/pull/813))
- *(gc)* replace sequential policy guards with exhaustive match ([#807](https://github.com/majiayu000/harness/pull/807)) ([#812](https://github.com/majiayu000/harness/pull/812))
- *(quality_trigger)* wrap gc_agent.run in tokio::time::timeout ([#806](https://github.com/majiayu000/harness/pull/806)) ([#811](https://github.com/majiayu000/harness/pull/811))
- *(learn)* repair broken feedback loop across scheduler, grader, GC gates ([#802](https://github.com/majiayu000/harness/pull/802))
- *(gc)* upgrade checkpoint failure logs from warn to error
- distinguish deny-all from full-access and add bypass-permissions for restricted mode
- *(sec)* harden data dir path construction against traversal (SEC-07, #510)
- remove --allowedTools flag that crashes Claude CLI 2.1.70 ([#483](https://github.com/majiayu000/harness/pull/483)) ([#493](https://github.com/majiayu000/harness/pull/493))
- *(gc)* validate artifact target_path within project root before writing
- eliminate silent error discards across codebase
- replace to_ascii_lowercase() with eq_ignore_ascii_case() for clippy compliance
- elide unnecessary lifetime annotation in filter_events_since
- resolve common module conflict between PR #167 and #168
- address OS sandbox rework feedback ([#156](https://github.com/majiayu000/harness/pull/156))
- validate artifact target_path in GC adopt ([#92](https://github.com/majiayu000/harness/pull/92))
- wire SignalThresholds from config instead of using default ([#101](https://github.com/majiayu000/harness/pull/101))
- wire rule violations through observability pipeline ([#52](https://github.com/majiayu000/harness/pull/52))

### Other

- *(release)* v0.6.34 ([#1196](https://github.com/majiayu000/harness/pull/1196))
- *(prompts)* centralise inline prompts under harness-core
- Fix audit findings ([#981](https://github.com/majiayu000/harness/pull/981))
- apply cargo fmt formatting
- checkpoint current harness workspace state
- *(gc)* add unit tests for SignalThresholds From<SignalThresholdsConfig> conversion ([#393](https://github.com/majiayu000/harness/pull/393))
- Merge pull request #261 from majiayu000/feat/parse-gc-artifacts-248
- rename SignalThresholds to SignalThresholdsConfig
- eliminate duplicate GcConfig definition
- Revert "fix: resolve common module conflict between PR #167 and #168"
- [P2] #111 TypeScript/Python SDK ([#157](https://github.com/majiayu000/harness/pull/157))
- Deduplicate signal priority mapping
- add unit tests for 8 crates (56 tests total)
