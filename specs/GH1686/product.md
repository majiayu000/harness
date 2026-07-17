# Product Spec

## Linked Issue

GH-1686

## User Problem

Harness maintainers cannot rely on pull-request validation when unrelated Rust
changes are blocked before project code runs. The `Web Build` dependency and
two other active workflows configure `oven-sh/setup-bun@v2` with
`bun-version: latest`. That resolution path enumerates the complete Bun tag
collection through the GitHub API. On 2026-07-17, the endpoint returned HTTP
503 in two consecutive exact-head runs for PR #1685, so web build, Clippy, and
tests never started even though the exact web build passed locally.

## Goals

- Make the Bun toolchain deterministic across all active workflows.
- Avoid the full Bun tag-discovery request during CI setup.
- Keep one repository-owned Bun version as the source of truth.
- Ensure a Bun version-only change triggers the workflows that validate it.
- Preserve all existing dependency-install, typecheck, test, build, artifact,
  and Rust validation commands.
- Restore reliable validation for unrelated Rust pull requests.

## Non-Goals

- Changing web or TypeScript dependencies, manifests, or lockfiles.
- Changing web, SDK, Rust production, or Rust test behavior.
- Adding a custom installer, mirror, retry loop, or fallback runtime.
- Updating GitHub Action revisions unrelated to Bun version resolution.
- Automating future Bun upgrades.

## User-Visible Behavior

There is no intended Harness product behavior change. CI installs the
repository-selected Bun 1.3.14 release instead of resolving `latest` on every
run. Existing web and Rust validation commands and artifacts remain unchanged.

## Acceptance Criteria

- [ ] B-001: A root `.bun-version` file contains exactly the strict semantic
      version `1.3.14` followed by one newline.
- [ ] B-002: `.github/workflows/ci.yml`,
      `.github/workflows/web-ci.yml`, and
      `.github/workflows/ci-disabled-modules.yml` configure
      `oven-sh/setup-bun@v2` through `bun-version-file: .bun-version`;
      `.bun-version` is also included in the main CI change filter and Web CI
      path triggers.
- [ ] B-003: No active workflow retains `bun-version: latest` or duplicates a
      literal Bun version.
- [ ] B-004: The strict version path constructs the release download without
      requesting `https://api.github.com/repos/oven-sh/bun/git/refs/tags`.
- [ ] B-005: Bun 1.3.14 completes frozen dependency installation, web
      typecheck, tests, SDK prebuild, and production build without manifest or
      lockfile changes.
- [ ] B-006: Apart from the narrow `.bun-version` path additions, existing
      workflow triggers, job dependencies, commands, artifact handling, and
      Rust validation behavior remain unchanged.
- [ ] B-007: Format, web validation, workflow syntax/scope checks, manual
      VibeGuard review, exact-head CI, review-thread audit, and SpecRail gates
      pass.
- [ ] B-008: The implementation is delivered in a separate PR only after this
      specification PR is merged.

## Edge Cases

- The version file must use a strict three-component semantic version; a
  range, `latest`, or malformed value would re-enter tag discovery.
- All three active Bun-using workflows must move together so pull-request,
  web-only, and all-features validation cannot drift.
- A pull request or push that changes only `.bun-version` must run main CI and
  Web CI instead of silently skipping validation for the selected toolchain.
- A cached runner and an uncached runner must resolve the same release.
- Existing frozen lockfiles must remain accepted by Bun 1.3.14.
- Historical examples under `docs/` are records, not active workflow
  configuration, and remain unchanged.

## Rollout Notes

This is a CI-toolchain reliability change. It needs no migration, feature flag,
operator action, or compatibility communication. Future Bun upgrades should
change `.bun-version` in a separately verified dependency-maintenance PR.
Squash-reverting the implementation PR restores the prior resolver behavior.
