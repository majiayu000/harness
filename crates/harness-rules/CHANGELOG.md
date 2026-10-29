# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.7.1](https://github.com/majiayu000/harness/releases/tag/harness-rules-v0.7.1) - 2026-10-21

### Added

- *(rules)* evaluate stack promotion policy ([#1944](https://github.com/majiayu000/harness/pull/1944))
- add RS-02B guard for SQL-layer TOCTOU detection
- wire rule auto-fix pattern into enforcement loop ([#273](https://github.com/majiayu000/harness/pull/273))
- add auto-fix pattern support to rule engine ([#247](https://github.com/majiayu000/harness/pull/247))
- *(rules)* auto-register builtin guard and warn on empty scans
- Phase 5 learn feedback loop for VibeGuard absorption
- wire thread persistence into router mutations (issue #18) ([#23](https://github.com/majiayu000/harness/pull/23))
- initial Harness platform — 10 Rust crates implementing OpenAI Harness Engineering

### Fixed

- keep skill, draft, and rule-fix joins inside their roots ([#2091](https://github.com/majiayu000/harness/pull/2091))
- *(rules)* fail closed on crashed guards and pin publish action SHAs ([#2080](https://github.com/majiayu000/harness/pull/2080))
- release read lock before async scan in rule_check to prevent writer starvation
- *(rules)* release read lock before async scan to prevent write starvation
- use per-task CARGO_TARGET_DIR to eliminate parallel build contention ([#488](https://github.com/majiayu000/harness/pull/488)) ([#498](https://github.com/majiayu000/harness/pull/498))
- replace TOCTOU get()+insert() with Entry API (RS-02)
- remove useless format! macro in engine test
- address OS sandbox rework feedback ([#156](https://github.com/majiayu000/harness/pull/156))
- add config source and policy APIs required by runtime wiring
- address high and medium security issues in PR #36
- merge duplicate test modules and fix rule id parsing for first section
- address review comments — propagate current_dir error and robust CSV parsing
- downgrade serde_yaml to 0.8 and add path format tests
- propagate serde_yaml error in parse_frontmatter_paths
- use serde_yaml to parse frontmatter paths field
- make parse_frontmatter_paths robust for string and array formats
- implement load_builtin with include_str! and parse frontmatter paths

### Other

- Connect Console v3 local quotas and rule operations ([#2116](https://github.com/majiayu000/harness/pull/2116))
- *(stack)* remove unused capability pipeline ([#2036](https://github.com/majiayu000/harness/pull/2036))
- Unify build warning policy and dev debug profile ([#1542](https://github.com/majiayu000/harness/pull/1542))
- release v0.6.34
- *(release)* v0.6.34 ([#1196](https://github.com/majiayu000/harness/pull/1196))
- Fix audit findings ([#981](https://github.com/majiayu000/harness/pull/981))
- checkpoint current harness workspace state
- split oversized router.rs and engine.rs into modules
- Merge pull request #279 from majiayu000/feat/rule-autofix-enforcement-loop-273
- Merge pull request #262 from majiayu000/feat/auto-fix-pattern-247
- Rework Starlark execpolicy engine (r3) ([#148](https://github.com/majiayu000/harness/pull/148))
- *(rules)* remove unused detect_languages loader
- add unit tests for 8 crates (56 tests total)
- use iter().any() in test instead of Vec collect+contains

## [0.6.34](https://github.com/majiayu000/harness/releases/tag/harness-rules-v0.6.34) - 2026-06-29

### Added

- add RS-02B guard for SQL-layer TOCTOU detection
- wire rule auto-fix pattern into enforcement loop ([#273](https://github.com/majiayu000/harness/pull/273))
- add auto-fix pattern support to rule engine ([#247](https://github.com/majiayu000/harness/pull/247))
- *(rules)* auto-register builtin guard and warn on empty scans
- Phase 5 learn feedback loop for VibeGuard absorption
- wire thread persistence into router mutations (issue #18) ([#23](https://github.com/majiayu000/harness/pull/23))
- initial Harness platform — 10 Rust crates implementing OpenAI Harness Engineering

### Fixed

- release read lock before async scan in rule_check to prevent writer starvation
- *(rules)* release read lock before async scan to prevent write starvation
- use per-task CARGO_TARGET_DIR to eliminate parallel build contention ([#488](https://github.com/majiayu000/harness/pull/488)) ([#498](https://github.com/majiayu000/harness/pull/498))
- replace TOCTOU get()+insert() with Entry API (RS-02)
- remove useless format! macro in engine test
- address OS sandbox rework feedback ([#156](https://github.com/majiayu000/harness/pull/156))
- add config source and policy APIs required by runtime wiring
- address high and medium security issues in PR #36
- merge duplicate test modules and fix rule id parsing for first section
- address review comments — propagate current_dir error and robust CSV parsing
- downgrade serde_yaml to 0.8 and add path format tests
- propagate serde_yaml error in parse_frontmatter_paths
- use serde_yaml to parse frontmatter paths field
- make parse_frontmatter_paths robust for string and array formats
- implement load_builtin with include_str! and parse frontmatter paths

### Other

- *(release)* v0.6.34 ([#1196](https://github.com/majiayu000/harness/pull/1196))
- Fix audit findings ([#981](https://github.com/majiayu000/harness/pull/981))
- checkpoint current harness workspace state
- split oversized router.rs and engine.rs into modules
- Merge pull request #279 from majiayu000/feat/rule-autofix-enforcement-loop-273
- Merge pull request #262 from majiayu000/feat/auto-fix-pattern-247
- Rework Starlark execpolicy engine (r3) ([#148](https://github.com/majiayu000/harness/pull/148))
- *(rules)* remove unused detect_languages loader
- add unit tests for 8 crates (56 tests total)
- use iter().any() in test instead of Vec collect+contains
