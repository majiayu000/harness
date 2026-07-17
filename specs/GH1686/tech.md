# Tech Spec

## Linked Issue

GH-1686

## Product Spec

See `specs/GH1686/product.md`.

## Current System

On base `807e129e746bb95afa9cba896d1c1916e7e6ea35`, three active workflows use
`oven-sh/setup-bun@v2` with `bun-version: latest`:

- `.github/workflows/ci.yml` builds the web bundle required by Rust Clippy and
  test jobs.
- `.github/workflows/web-ci.yml` runs web typecheck, tests, and build.
- `.github/workflows/ci-disabled-modules.yml` builds the web bundle before the
  all-features workspace check.

The upstream action first accepts `bun-version`, then `bun-version-file`, then
the package manifest. Its download resolver recognizes a strict semantic
version with `validateStrict` and directly constructs the corresponding
`bun-v<version>` release URL. For `latest`, ranges, or other unresolved values,
it requests the complete `oven-sh/bun` tag collection from the GitHub API.

PR #1685 run `29539326830` failed twice in `setup-bun` jobs `87757987147` and
`87758451029`: the tags request returned HTTP 503 before any Harness build
command ran. The exact local `bun install --frozen-lockfile && bun run build`
command passed. The Bun 1.3.14 Linux release asset resolves successfully.

## Proposed Design

Add a root `.bun-version` containing:

```text
1.3.14
```

Replace each active workflow's `bun-version: latest` input with:

```yaml
bun-version-file: .bun-version
```

Add `.bun-version` to the `ci` filter in `.github/workflows/ci.yml` so a
version-only pull request runs the web bundle, Clippy, tests, and CI result.
Add the same path to `.github/workflows/web-ci.yml` so version-only pull
requests and pushes run web typecheck, tests, build, and artifact upload.
`ci-disabled-modules.yml` already runs for every pull request and needs no
trigger expansion.

The action reads the trimmed file value, recognizes it as a strict semantic
version, sets `tag = bun-v1.3.14`, and constructs the release asset URL without
executing its tag-enumeration request. A root file is used because
`setup-bun` resolves version files from `GITHUB_WORKSPACE`, including when
later run steps use `working-directory: web`.

## Invariants

- `.bun-version` is the only active Bun version source and contains exactly
  `1.3.14` plus one newline.
- Every active `oven-sh/setup-bun@v2` step uses the same root version file.
- No active workflow uses `latest`, a range, a literal duplicated version, a
  custom download URL, or a fallback installer.
- The main CI `ci` change filter and Web CI paths include `.bun-version`;
  all other triggers, filters, permissions, runners, dependencies, environment,
  working directories, commands, artifact paths, and retention remain exact.
- Web and SDK manifests and lockfiles remain byte-for-byte unchanged.
- No Rust source or test file changes.

## Affected Files

- `.bun-version`
- `.github/workflows/ci.yml`
- `.github/workflows/web-ci.yml`
- `.github/workflows/ci-disabled-modules.yml`

No other configuration, workflow, dependency, manifest, lockfile, production,
test, persistence, schema, or serialized-format file is expected to change.

## Data Flow

At workflow startup, `setup-bun` reads `.bun-version` from
`GITHUB_WORKSPACE`, validates `1.3.14` strictly, derives the exact Bun release
asset URL, restores or downloads that binary, and exposes it to unchanged web
and Rust validation steps. No Harness runtime data flow changes.

## Alternatives Considered

- Keep `latest` and rerun failures: rejected because it leaves unrelated PRs
  dependent on an avoidable full-tag API request and makes toolchain results
  non-deterministic.
- Add retries around setup: rejected because the action fails inside its own
  JavaScript step and retries would retain the unstable resolver and increase
  CI latency.
- Repeat `bun-version: 1.3.14` in every workflow: rejected because three
  literals can drift and make upgrades error-prone.
- Use `bun-download-url`: rejected because platform-specific URLs bypass the
  action's supported version selection and complicate portability.
- Put the version in `web/package.json`: rejected because `engines.bun`
  currently describes compatibility (`>=1.0.0`), not a repository toolchain
  pin, and changing it would mix package contract and CI selection semantics.
- Preserve the existing path filters without `.bun-version`: rejected because
  a later version-only change would select a new executable while skipping the
  main and web validation intended to qualify it.

## Risks

- Security: low; the existing official action and official release assets are
  retained, with no secrets, authentication, shell construction, or custom
  executable source introduced. Workflow changes require human review under
  SEC-11 because they control downloaded tooling.
- Compatibility: low; Bun 1.3.14 is a published release and frozen install,
  typecheck, tests, SDK prebuild, and production build are required before
  merge.
- Availability: improved by eliminating the tag-list API request; release
  asset availability remains an unavoidable external dependency.
- Maintenance: future Bun upgrades become explicit one-line changes with
  deterministic review and validation.
- Staleness: a pin does not auto-upgrade; intentional dependency maintenance
  is preferable to silently changing CI toolchains.

## Test Plan

- [ ] Verify the Bun 1.3.14 release asset resolves and the downloaded binary
      reports `1.3.14`.
- [ ] With Bun 1.3.14, run `bun install --frozen-lockfile`,
      `bun run typecheck`, `bun run test`, and `bun run build` from `web/`.
- [ ] Run `cargo fmt --all -- --check` and
      `cargo check --workspace --all-features` after the web bundle exists.
- [ ] Parse all changed YAML workflows and inspect the exact diff.
- [ ] Assert one version file, three `bun-version-file` references, zero active
      `bun-version: latest` references, the two required `.bun-version` filter
      entries, unchanged manifests/lockfiles, and the approved four-file scope.
- [ ] Confirm a `.bun-version`-only path classification selects main CI and
      Web CI while unrelated trigger/filter behavior remains unchanged.
- [ ] Run repository-required workspace Clippy/tests, SpecRail checks, manual
      VibeGuard L1-L7 review, exact-head CI, review-thread audit, and required
      PR gate.

## Rollback Plan

Squash-revert the implementation PR. No data, schema, migration, application
configuration, or operator rollback step is required. If Bun 1.3.14 itself is
found incompatible before merge, stop and update this spec with a verified
non-downgrade version rather than restoring `latest` silently.
