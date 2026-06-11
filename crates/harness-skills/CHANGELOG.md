# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.6.34](https://github.com/majiayu000/harness/releases/tag/harness-skills-v0.6.34) - 2026-06-11

### Added

- *(skills)* add rebase-pr playbook skill
- *(skills)* add freshness classifier and skill/stale endpoint ([#602](https://github.com/majiayu000/harness/pull/602))
- add skill usage statistics tracking
- add skill versioning with change tracking
- skill persistence, trigger patterns, and auto-injection ([#220](https://github.com/majiayu000/harness/pull/220))
- embed 10 built-in skills (Phase 3 VibeGuard absorption)
- persist SkillStore creates/deletes to disk (issue #19) ([#24](https://github.com/majiayu000/harness/pull/24))
- initial Harness platform — 10 Rust crates implementing OpenAI Harness Engineering

### Fixed

- *(agents)* pipe prompt via stdin instead of positional CLI argument
- replace TOCTOU get()+insert() with Entry API (RS-02)
- restore User location for persist_dir skills to preserve shadowing behavior
- assign System location to skills loaded from persist_dir
- assign User location to skills loaded from persist_dir
- address OS sandbox rework feedback ([#156](https://github.com/majiayu000/harness/pull/156))
- use unreachable! in store.rs create() per Gemini review
- eliminate unwrap/expect — dispatch() returns Result

### Other

- *(release)* v0.6.34 ([#1196](https://github.com/majiayu000/harness/pull/1196))
- use skill helper in store tests
- *(harness-skills/store)* extract tests via #[path] ([#1093](https://github.com/majiayu000/harness/pull/1093))
- apply cargo fmt formatting
- checkpoint current harness workspace state
- document startup sequence for with_persist_dir in SkillStore
- add discover_reloads_persisted_skills_across_restart
- prune dead modules and tighten harness-server test sandboxes
- add unit tests for 8 crates (56 tests total)
