# Product Spec

## Linked Issue

GH-1670

## User Problem

Harness maintainers must navigate an 822-line `plan_db.rs` file even though
the production persistence implementation ends at line 388 and the remaining
434 lines are a cohesive inline test module. The combined file exceeds the
repository 800-line hard ceiling and mixes production database logic with 15
database, migration, transaction, and search tests.

## Goals

- Give the existing plan database tests a dedicated private child module while
  retaining their current parent module and test names.
- Reduce both resulting Rust files to no more than 800 formatted lines.
- Preserve the production implementation exactly and preserve every test's
  non-whitespace Rust tokens, assertion, fixture, SQL statement, attribute,
  and coverage point; only deterministic whitespace reflow required by
  `rustfmt` is permitted.
- Make plan persistence changes reviewable without traversing the complete
  database test suite in the same physical source file.
- Provide deterministic evidence that the extraction is mechanical.

## Non-Goals

- Changing plan creation, lookup, update, delete, search, migration, backfill,
  transaction, schema, store-key, or NUL-byte sanitization behavior.
- Adding, deleting, renaming, consolidating, or rewriting tests.
- Changing SQL, migrations, public APIs, private production visibility,
  configuration, dependencies, manifests, lockfiles, or persisted formats.
- Addressing unrelated plan database behavior or coverage findings.

## User-Visible Behavior

There is no intended user-visible behavior change. Plan persistence, filtering,
search, transaction updates, legacy backfill, schema migration, deletion, and
error behavior must remain identical to current `origin/main`.

## Acceptance Criteria

- [ ] B-001: `plan_db.rs` and the extracted test module each contain no more
      than 800 formatted lines.
- [ ] B-002: The complete production function/type/constant inventory and
      every production item body remain unchanged.
- [ ] B-003: All 15 test names, non-whitespace Rust tokens, assertions,
      fixtures, SQL statements, attributes, ordering, and
      `plan_db::tests::*` module paths remain unchanged. Whitespace may differ
      only when the unindented baseline test body is canonically formatted by
      the repository `rustfmt` version.
- [ ] B-004: No production behavior, public API, visibility, dependency,
      manifest, lockfile, SQL, migration, schema, or persisted-format change.
- [ ] B-005: Focused plan database tests, `harness-workflow` checks/tests,
      workspace Clippy/tests, VibeGuard, exact-head CI, review-thread audit,
      and SpecRail gates pass.
- [ ] B-006: The implementation is delivered in a separate PR only after this
      spec PR is merged.

## Edge Cases

- Tests must retain child-module access to private migration arrays, helpers,
  pools, transaction methods, and store keys through the existing
  `use super::*` relationship without widening production visibility.
- PostgreSQL schema names, SQL strings, JSON fixtures, NUL-byte inputs,
  markdown migration fixtures, assertions, and cleanup behavior must move
  without token rewriting; indentation-driven `rustfmt` line wrapping is
  permitted.
- Test filtering by the existing `plan_db::tests::...` path must continue to
  select the same 15 tests.
- The module declaration remains test-only and must not affect production
  builds or exported symbols.

## Rollout Notes

This is a test-source layout refactor only. It requires no data migration,
feature flag, configuration change, operator action, or compatibility
communication. Squash-reverting the implementation PR is the rollback path.
