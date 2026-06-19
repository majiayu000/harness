# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.6.34](https://github.com/majiayu000/harness/releases/tag/harness-observe-v0.6.34) - 2026-06-19

### Added

- *(observe)* wire token usage overview KPIs
- *(skill-injection)* inject skills into triage, plan, and review agent prompts ([#979](https://github.com/majiayu000/harness/pull/979))
- *(db)* migrate remaining SQLite stores to Postgres (closes #747) ([#831](https://github.com/majiayu000/harness/pull/831))
- *(scheduler)* periodic retry loop for stalled open issues ([#794](https://github.com/majiayu000/harness/pull/794)) ([#796](https://github.com/majiayu000/harness/pull/796))
- *(observe)* add LLM-specific metrics (cost/turns/linter/latency) ([#738](https://github.com/majiayu000/harness/pull/738))
- *(observe)* wire periodic purge timer for event store ([#719](https://github.com/majiayu000/harness/pull/719))
- *(quality)* wire challenger-agent LLM judge into QualityTrigger ([#637](https://github.com/majiayu000/harness/pull/637)) ([#654](https://github.com/majiayu000/harness/pull/654))
- add content field to Event for persisting full reviewer output
- migrate EventStore from JSONL to SQLite
- add external signal ingestion for CI failures and review feedback
- wire rule auto-fix pattern into enforcement loop ([#273](https://github.com/majiayu000/harness/pull/273))
- auto-trigger GC based on QualityGrader score ([#245](https://github.com/majiayu000/harness/pull/245))
- add GET /health endpoint ([#48](https://github.com/majiayu000/harness/pull/48))
- Phase 6 health report and stats for VibeGuard absorption
- initial Harness platform — 10 Rust crates implementing OpenAI Harness Engineering

### Fixed

- *(storage)* make path schema openings explicit ([#1364](https://github.com/majiayu000/harness/pull/1364))
- *(deps)* resolve tracked cargo audit advisories ([#932](https://github.com/majiayu000/harness/pull/932))
- *(db)* replace DefaultHasher with SHA-256 for stable Postgres schema names ([#869](https://github.com/majiayu000/harness/pull/869))
- replace read_to_string with BufReader streaming in event replay ([#657](https://github.com/majiayu000/harness/pull/657)) ([#661](https://github.com/majiayu000/harness/pull/661))
- *(observe)* restore poison recovery on otel_pipeline mutex lock sites
- *(observe)* use std::sync::Mutex for otel_pipeline, lock not held across await
- *(observe)* replace std::sync::Mutex with tokio::sync::Mutex for otel_pipeline
- resolve CI failures — fmt, import paths, and clippy lints
- correct test import paths after pub use re-exports were removed
- *(sec)* address reviewer issues from PR #564
- *(sec)* harden data dir path construction against traversal (SEC-07, #510)
- prevent content blob exposure and hot-path memory bloat
- wire ObserveConfig fields into EventStore
- eliminate silent error discards across codebase
- surface session file io errors in session manager
- add EventStore::close() to prevent test hangs
- stream EventStore query instead of reading entire file into memory
- address OS sandbox rework feedback ([#156](https://github.com/majiayu000/harness/pull/156))
- rework OpenTelemetry export with async-safe transport checks ([#149](https://github.com/majiayu000/harness/pull/149))
- add config source and policy APIs required by runtime wiring
- wire rule violations through observability pipeline ([#52](https://github.com/majiayu000/harness/pull/52))

### Other

- Migrate event store to shared schema ([#1350](https://github.com/majiayu000/harness/pull/1350))
- *(release)* v0.6.34 ([#1196](https://github.com/majiayu000/harness/pull/1196))
- Add structured turn telemetry to task and event surfaces ([#915](https://github.com/majiayu000/harness/pull/915))
- Improve Postgres startup health reporting
- Resolve database URL from Harness config ([#995](https://github.com/majiayu000/harness/pull/995))
- Fix audit findings ([#981](https://github.com/majiayu000/harness/pull/981))
- Prevent expired capability tokens from reaching Anthropic agent calls
- Separate background review load from issue intake capacity
- *(db)* validate schema name before SQL interpolation ([#862](https://github.com/majiayu000/harness/pull/862))
- *(event_store)* skip JSONL replay when events table is already populated ([#851](https://github.com/majiayu000/harness/pull/851))
- *(learn)* per-agent scan watermark in QualityTrigger ([#822](https://github.com/majiayu000/harness/pull/822))
- *(task_runner)* add coverage for dependency scheduling ([#634](https://github.com/majiayu000/harness/pull/634)) ([#785](https://github.com/majiayu000/harness/pull/785))
- checkpoint current harness workspace state
- Merge pull request #361 from majiayu000/feat/fix-silent-error-discards
- [Critical] 合并 violation 持久化双通道并统一 rule_scan 锚点 ([#58](https://github.com/majiayu000/harness/pull/58))
- Remove unused observe session/metrics definitions ([#66](https://github.com/majiayu000/harness/pull/66))
- add unit tests for 8 crates (56 tests total)
