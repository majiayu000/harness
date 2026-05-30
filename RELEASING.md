# Releasing Harness

This document describes how to cut a Harness release and (optionally) publish the
workspace crates to crates.io.

Harness is a Cargo workspace of 13 crates that share a single version
(`[workspace.package] version` in the root `Cargo.toml`). All crates are released
together under one version number.

## Versioning

Harness follows [Semantic Versioning](https://semver.org/). For a `0.x` series:

- **patch** (`0.6.33 -> 0.6.34`): backward-compatible bug fixes.
- **minor** (`0.6.x -> 0.7.0`): new features, or behavior changes that callers
  may need to adapt to.
- **major**: reserved for the eventual `1.0.0`.

Feature and fix PRs must **not** bump the version. The version is bumped only at
release time, in a dedicated release PR, to avoid cascading merge conflicts
across parallel PRs.

## Cutting a release

1. Make sure `main` is green and contains everything you want to ship.
2. Create a release branch from `main`:
   ```bash
   git fetch origin main && git checkout -b release/vX.Y.Z origin/main
   ```
3. Bump the workspace version in the root `Cargo.toml`
   (`[workspace.package] version`). If you publish to crates.io, also bump the
   `version = "X.Y.Z"` on each internal entry under `[workspace.dependencies]`
   (they must match the workspace version).
4. Update `CHANGELOG.md`: add a new `## [X.Y.Z] - YYYY-MM-DD` section grouped by
   `Added` / `Changed` / `Fixed`, and reference the relevant PRs/issues.
5. Verify locally:
   ```bash
   cargo fmt --all -- --check
   RUSTFLAGS="-Dwarnings" cargo check --workspace --all-targets
   cargo test --workspace
   ```
6. Commit (`-s` for DCO), open a PR titled `chore(release): vX.Y.Z`, wait for CI
   + review, then squash-merge.
7. Tag the squash-merge commit and push the tag:
   ```bash
   git fetch origin main
   git tag -a vX.Y.Z <merge-commit-sha> -m "vX.Y.Z"
   git push origin vX.Y.Z
   ```
8. Create the GitHub release:
   ```bash
   gh release create vX.Y.Z --title "vX.Y.Z" --notes-file <notes.md> --latest
   ```

## Publishing to crates.io (optional)

The crates are publish-ready (they carry `description`, `license`, `repository`,
and versioned internal dependencies), but publishing is a separate, manual,
**irreversible** step (a published version can only be yanked, not removed).

> **Status (2026-05-30): Harness is not published to crates.io, and there is no
> current plan to.** Two of the workspace crate names are already owned by
> unrelated projects — `harness-core` (owner: `avifenesh`) and `harness-cli`
> (owner: `wenyuzhao`, a benchmarking tool) — so the workspace cannot be
> published under its current names. Harness is an internal agent-orchestration
> application rather than a reusable library, so the intended distribution path
> is **GitHub releases** plus `cargo install --git https://github.com/majiayu000/harness`,
> not crates.io. Revisit only if there is a concrete need to consume these crates
> as external dependencies; that would first require renaming all crates to an
> available namespace (keep `[lib] name = "harness_*"` so Rust `use` paths stay
> unchanged, and only change `package.name` + the internal `[workspace.dependencies]`
> entries).

The remainder of this section documents the procedure should that decision change.

### Prerequisites

1. Run from an environment with crates.io network access.
2. `cargo login <token>` with the account that should own the crates.
3. **Confirm crate-name ownership/availability first.** The `harness-*` names are
   generic and may already be taken by other projects:
   ```bash
   for c in harness-core harness-exec harness-observe harness-protocol harness-rules \
            harness-sandbox harness-skills harness-workflow harness-agents harness-api \
            harness-gc harness-server harness-cli; do
     printf "%s -> " "$c"; curl -s -o /dev/null -w "%{http_code}\n" "https://crates.io/api/v1/crates/$c"
   done
   # 404 = available; 200 = exists (confirm it is yours, otherwise rename/namespace)
   ```
4. Check out the released tag: `git checkout vX.Y.Z`.

### Publish order

Publish in dependency (topological) order, waiting for the index to update
between crates. The order for the current workspace is:

```
1. harness-core        8. harness-workflow
2. harness-exec        9. harness-agents
3. harness-observe    10. harness-api
4. harness-protocol   11. harness-gc
5. harness-rules      12. harness-server
6. harness-sandbox    13. harness-cli
7. harness-skills
```

Dry-run a leaf first, then publish:

```bash
cargo publish -p harness-core --dry-run
cargo publish -p harness-core
# ... continue in the order above, allowing the index to refresh between crates
```

Or use a tool that resolves order and waits automatically:

```bash
cargo install cargo-workspaces
cargo workspaces publish --from-git
```

> If any `harness-*` name is already owned by someone else, publishing is blocked
> until the affected crate is renamed (e.g. to a reserved namespace). Decide on a
> naming strategy before the first publish.
