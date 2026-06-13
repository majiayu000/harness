# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.6.34](https://github.com/majiayu000/harness/releases/tag/harness-exec-v0.6.34) - 2026-06-13

### Added

- extract database abstraction layer to harness-core
- initial Harness platform — 10 Rust crates implementing OpenAI Harness Engineering

### Fixed

- address OS sandbox rework feedback ([#156](https://github.com/majiayu000/harness/pull/156))

### Other

- *(release)* v0.6.34 ([#1196](https://github.com/majiayu000/harness/pull/1196))
- Convert generic Db store to Postgres ([#1030](https://github.com/majiayu000/harness/pull/1030))
- *(task_runner)* add coverage for dependency scheduling ([#634](https://github.com/majiayu000/harness/pull/634)) ([#785](https://github.com/majiayu000/harness/pull/785))
- checkpoint current harness workspace state
- add unit tests for 8 crates (56 tests total)
