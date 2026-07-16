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

5. Activate local git hooks:

```bash
git config core.hooksPath .githooks
```

The pre-commit hook is intentionally fast: it runs `cargo fmt --all -- --check`
and clippy for the cargo packages touched by staged files. Docs-only staged
changes skip clippy.

The pre-push hook is the full local gate before upload: it runs
`cargo clippy --workspace --all-targets -- -D warnings` in every mode. Without
`HARNESS_DATABASE_URL`, it runs database-independent workspace and
`harness-workflow` lib tests under an isolated config root, then defers the
explicit PostgreSQL suites to CI or a configured local database. With an
isolated disposable database configured through `HARNESS_DATABASE_URL`, it
runs the full `harness-workflow` and `harness-server` lib suites.

Use the smallest focused check that covers the files you changed before moving
to broader gates:

| Change type | Start with | Add before handoff |
| --- | --- | --- |
| Docs-only | `cargo test --help >/dev/null` | Add the command documented by the changed page if it names one. No Postgres is required for text-only edits. |
| CLI (`crates/harness-cli`) | `cargo test -p harness-cli --all-targets` | Add the touched library crate test when the command delegates into shared logic. |
| Server, API, or workflow runtime | `HARNESS_DATABASE_URL=postgres://harness:harness@localhost:5432/harness_test scripts/test-server-fast.sh` | Postgres is required. Use `scripts/test-server-db.sh` for startup, recovery, persistence, full `AppState`, route, or workflow-runtime changes. |
| SDK (TypeScript) (`sdk/typescript`) | `cd sdk/typescript && bun install && bun run test && bun run build` | Run the package-local build before publishing or release work. |
| SDK (Python) (`sdk/python`) | `python3 -m unittest discover sdk/python/tests` | Run `cd sdk/python && python3 -m build` before publishing or release work. |
| Web (`web/`) | `cd web && bun install && bun run typecheck && bun run test` | Run `cd web && bun run build` when routes, generated SDK types, or production assets changed. |
| Workflow/spec pack | `python3 checks/check_workflow.py --repo .` | For a numbered spec packet, also run `python3 checks/check_workflow.py --repo . --spec-dir specs/GH<number>`. Run `python3 -m pytest tests/test_evaluate.py` when checks or templates change. |

For `harness-server` work, start with the fast local ladder and reserve the
full DB profile for final handoff or changes that touch startup, recovery,
full `AppState`, route persistence, or workflow runtime behavior:

```bash
HARNESS_DATABASE_URL=postgres://harness:harness@localhost:5432/harness_test scripts/test-server-fast.sh
HARNESS_DATABASE_URL=postgres://harness:harness@localhost:5432/harness_test scripts/test-server-db.sh
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
