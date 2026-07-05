# Product Spec

## Linked Issue

GH-1458

## User Problem

Harness currently uses multiple Rust warning-policy build universes that share
the same `target/debug` directory. Normal developer checks run without
`RUSTFLAGS`, while hooks and CI inject `RUSTFLAGS=-Dwarnings`. Switching between
those environments invalidates Cargo fingerprints and forces expensive rebuilds,
which slows every agent and maintainer loop.

The repository should express warning strictness in the workspace manifests so
local checks, hooks, and CI compile under one consistent universe.

## Goals

- Move deny-warnings behavior from environment variables into the Cargo
  workspace lint configuration.
- Ensure every workspace crate opts into the shared lint policy.
- Remove functional `RUSTFLAGS=-Dwarnings` usage from hooks, workflows, and
  active developer commands.
- Preserve CI-equivalent warning strictness for `cargo clippy` and normal
  workspace checks.
- Coordinate implementation timing with GH-1459 so the repository pays one
  intended rebuild for both build-profile changes.

## Non-Goals

- Changing debug-info or profile settings; GH-1459 owns that behavior.
- Restructuring the pre-commit or pre-push gate split; GH-1464 owns that
  workflow change.
- Editing historical docs that mention prior `RUSTFLAGS` commands only as
  archival evidence.
- Weakening warning failures to improve short-term build speed.

## User-Visible Behavior

Developers, hooks, and CI run the same warning policy without setting
`RUSTFLAGS`. A clean tree passes `cargo clippy --workspace --all-targets` with
no warning-related environment override. If a new Rust warning is introduced in
workspace code, the normal workspace lint policy fails the relevant local or CI
command instead of relying on a separate `RUSTFLAGS` universe.

After a clean build, alternating between workspace check and clippy should reuse
the same fingerprint universe. Repeating the same check/clippy sequence should
not recompile unchanged crates just because the warning policy moved between
the environment and manifest.

## Acceptance Criteria

- [ ] The root manifest defines workspace-level Rust warning denial with
      `[workspace.lints.rust] warnings = "deny"`.
- [ ] Every workspace crate manifest opts into workspace lint inheritance with
      `[lints] workspace = true`.
- [ ] Functional hook, workflow, script, and active developer-command uses of
      `RUSTFLAGS=-Dwarnings` are removed or replaced with equivalent plain
      cargo commands.
- [ ] CI and local verification still fail on an intentionally introduced Rust
      warning and pass again after reverting that warning.
- [ ] `cargo check --workspace --all-targets` and
      `cargo clippy --workspace --all-targets` run without warning-policy
      environment variables.
- [ ] A fingerprint-stability verification runs check, clippy, then repeats
      both commands and records whether the second round avoids unnecessary
      recompilation.
- [ ] Documentation and PR notes call out that local inner-loop checks now
      fail warnings earlier than before.

## Edge Cases

- Clippy-specific warnings remain governed by clippy's command-line settings
  such as `-- -D warnings`; this spec only removes environment-level
  `RUSTFLAGS=-Dwarnings`.
- Historical docs may continue to mention old commands when they describe past
  incidents or preserved evidence, but active setup, hook, workflow, and agent
  instructions must not prescribe the old environment override.
- Crates added later must opt into workspace lint inheritance; otherwise they
  would silently leave the shared warning policy.
- Build scripts and generated code must either be warning-clean or updated so
  the manifest-level policy does not create a new CI-only failure.

## Rollout Notes

This is a build-behavior change. It intentionally moves warning failures
earlier into normal local development. Implement GH-1458 and GH-1459 together
or in immediately adjacent PRs so maintainers pay one expected rebuild when the
workspace fingerprint universe and debug-info profile both change.
