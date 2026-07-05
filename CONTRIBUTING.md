# Contributing to Harness

Thanks for contributing.

## Development Setup

1. Install Rust `1.88+`.
2. Clone the repository.
3. Build the workspace:

```bash
cargo build
```

4. Run tests before opening a PR:

```bash
cargo test
```

For `harness-server` work, start with the fast local ladder and reserve the
full DB profile for final handoff or changes that touch startup, recovery,
full `AppState`, route persistence, or workflow runtime behavior:

```bash
HARNESS_DATABASE_URL=postgres://harness:harness@localhost:5432/harness scripts/test-server-fast.sh
HARNESS_DATABASE_URL=postgres://harness:harness@localhost:5432/harness scripts/test-server-db.sh
```

## Target Directory Cleanup

Cargo build artifacts can accumulate across the default `target/` directory and
auxiliary `target/cargo-*` target directories. Use the manual target GC command
when disk usage grows unexpectedly or before large branch changes:

```bash
make gc-target
```

The command keeps artifacts newer than 14 days by default, prints before/after
size reports, and summarizes freed bytes. Use a custom retention window when
needed:

```bash
scripts/gc-target.sh --days 7
scripts/gc-target.sh --days 30 --dry-run
```

If `cargo sweep` is installed, the script prefers it for the default Cargo
target directory. Otherwise it falls back to bounded mtime cleanup under Cargo
artifact subdirectories. Run this manually when no Cargo build is active;
cleaning target directories during an active build can force rebuilds or
conflict with Cargo's build directory activity.

## Pull Request Guidelines

1. Keep PRs focused and small when possible.
2. Include clear problem statement and scope in the PR description.
3. Add or update tests for behavior changes.
4. Update docs (`README.md`, `docs/`, command help text) when interfaces change.
5. Ensure CI checks pass.

## Commit Style

- Use imperative commit messages.
- Prefer one logical change per commit.

## Reporting Bugs

Open a GitHub issue with:

- expected behavior
- actual behavior
- reproduction steps
- environment details (OS, Rust version, command used)
