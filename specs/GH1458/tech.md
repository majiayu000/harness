# Tech Spec

## Linked Issue

GH-1458

## Product Spec

See `specs/GH1458/product.md`.

## Current System

| Area | Files | Current behavior | Why relevant |
| --- | --- | --- | --- |
| Workspace manifest | `Cargo.toml` | The root manifest defines workspace members and dependencies but does not define `[workspace.lints]`. | This is the canonical place for shared Cargo lint policy. |
| Crate manifests | `crates/*/Cargo.toml` | Workspace crates do not currently opt into a shared lint table. | Cargo requires each package to opt into inherited workspace lints. |
| GitHub CI | `.github/workflows/ci.yml`, `.github/workflows/ci-disabled-modules.yml` | CI injects `RUSTFLAGS=-Dwarnings` globally. | This creates a separate Cargo fingerprint universe from local no-env checks. |
| Local hooks | `.githooks/pre-commit` | The hook invokes clippy with a warning-deny environment override. | Hook runs invalidate or reuse different build artifacts than normal checks. |
| Agent and developer docs | `AGENTS.md`, `CLAUDE.md`, `README.md`, active workflow docs | Some active commands still describe `RUSTFLAGS=-Dwarnings`. | Active instructions should match the manifest-based policy. |
| Historical docs | `docs/**/*.md` | Several older specs and audit notes mention `RUSTFLAGS` as past evidence. | These are not functional command sources and may stay unless they mislead active workflow usage. |

## Proposed Design

1. Add a root workspace lint policy:

   ```toml
   [workspace.lints.rust]
   warnings = "deny"
   ```

2. Add workspace lint inheritance to every workspace crate manifest:

   ```toml
   [lints]
   workspace = true
   ```

   Apply this to each package listed under `crates/*/Cargo.toml`. Search for
   package manifests rather than assuming the crate count.

3. Remove functional `RUSTFLAGS=-Dwarnings` usage from active execution paths:
   - `.github/workflows/ci.yml`
   - `.github/workflows/ci-disabled-modules.yml`
   - `.githooks/pre-commit`
   - active repo instructions such as `AGENTS.md` and `CLAUDE.md`
   - scripts or docs that operators are expected to run as current commands

4. Preserve clippy strictness with clippy's own command-line lint setting where
   that is already the repo standard:

   ```sh
   cargo clippy --workspace --all-targets -- -D warnings
   ```

   This is not the same as `RUSTFLAGS=-Dwarnings`; it does not create a separate
   rustc environment fingerprint universe.

5. Keep GH-1459 coordinated. If the implementation branch also changes
   `[profile.dev]`, the PR may close both GH-1458 and GH-1459 only after both
   spec packets and acceptance criteria are satisfied.

## Data Flow

Cargo reads the root workspace lint table, each crate opts into inherited lints,
and the same warning policy is applied during check, clippy, test builds, hooks,
and CI without a warning-policy environment variable. CI and local hooks invoke
plain cargo commands for rustc warning policy, with clippy-specific denial left
as a clippy argument.

## Alternatives Considered

- Keep `RUSTFLAGS=-Dwarnings` and accept the rebuild cost. Rejected because it
  preserves the fingerprint split this issue is meant to remove.
- Move `rustflags = ["-Dwarnings"]` into `.cargo/config.toml`. Rejected because
  it still models warning policy as compiler flags rather than package lint
  policy and is easier to miss in review.
- Remove all `-D warnings` spellings, including clippy's command-line lint
  flag. Rejected because clippy-specific warning denial is part of the current
  CI-equivalent gate and does not create the same rustc environment split.

## Risks

- Security: no direct security surface; changes are limited to build policy and
  instructions.
- Compatibility: developers may see warning failures earlier in normal local
  checks. This is intended CI parity but should be called out in docs.
- Performance: the first build after the manifest change will rebuild the
  workspace. Repeated check/clippy cycles should be faster after the one-time
  rebuild.
- Maintenance: new crates must remember `[lints] workspace = true`; verification
  should grep package manifests to prevent drift.

## Test Plan

- [ ] Manifest audit: every workspace crate manifest contains
      `[lints] workspace = true`.
- [ ] Search audit: no functional `RUSTFLAGS=-Dwarnings` usage remains in hooks,
      workflows, scripts, or active instructions.
- [ ] Negative warning check: temporarily introduce a Rust warning, verify the
      no-env workspace command fails, then revert the warning before commit.
- [ ] Local build checks: `cargo check --workspace --all-targets` and
      `cargo clippy --workspace --all-targets -- -D warnings` pass.
- [ ] Fingerprint check: run check, clippy, then repeat both and record the
      second-round no-op behavior or timing evidence.
- [ ] SpecRail checks: `python3 checks/check_workflow.py --repo .
      --spec-dir specs/GH1458` and `python3 checks/check_workflow.py --repo .`
      pass.

## Rollback Plan

Revert the manifest lint additions and restore the previous hook/workflow
environment variables. This returns the repo to the old split fingerprint
universe and does not require data migration.
