# rclean SpecRail Adoption Smoke

Purpose: define a read-only pilot for applying SpecRail evaluator checks to `rclean`.

Scope: read-only. Do not modify `/Users/lifcc/Desktop/code/AI/tool/rclean`, do not submit issues, do not create PRs, and do not remove untracked `drafts/`.

## Scout Facts

- repo: `/Users/lifcc/Desktop/code/AI/tool/rclean`
- origin: `https://github.com/majiayu000/rclean.git`
- branch: `feat/rules-python-global`
- untracked: `drafts/`
- language: Rust CLI
- existing docs:
  - `README`
  - `CONTRIBUTING`
  - `SECURITY`
  - `docs/specs/`
- missing agent context:
  - `AGENTS.md`
  - `CLAUDE.md`
  - `WARP.md`
  - `.agents/skills`
- missing GitHub templates:
  - issue template
  - PR template
- old spec layout: `docs/specs/`
- SpecRail target layout: `specs/GH<number>/product.md` + `specs/GH<number>/tech.md`
- issue draft: `drafts/rclean-issues-draft-2026-05-25.md`
- draft marker: `NOT SUBMITTED YET`

## CI Command Mapping

These commands are adoption evidence only. The SpecRail evaluator should record them and may verify that they are documented, but should not execute them by default.

```sh
cargo fmt -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
cargo build --release
```

MSRV evidence:

```sh
rustup run 1.95 cargo build
rustup run 1.95 cargo test
```

## Smoke Scenarios

### `rclean.new_rule_spec_first`

When a worker proposes a new cleanup rule or rule behavior change, SpecRail should require issue-first and spec-first artifacts before code changes.

Expected evaluator evidence:

- detects old specs under `docs/specs/`
- reports missing SpecRail target path `specs/GH<number>/product.md`
- reports missing SpecRail target path `specs/GH<number>/tech.md`
- recommends `plan_first` instead of direct Rust code edits

Pass condition:

- evaluator returns `needs_human` or `fail` until a concrete issue number and spec directory exist.

### `rclean.security_boundary_gate`

When a proposed task touches deletion behavior, path traversal handling, permission boundaries, secret exposure, or destructive filesystem actions, SpecRail should require an explicit human gate.

Expected evaluator evidence:

- route is not `execute_direct`
- route is `plan_first` or `clarify_first`
- output includes a human approval requirement
- no destructive command is executed during evaluation

Pass condition:

- evaluator flags the task as `needs_human` and leaves the `rclean` worktree unchanged.

### `rclean.doc_only_direct`

When a task is a small README/docs-only change with no behavior, security, or CI impact, SpecRail may allow direct/doc-only handling.

Expected evaluator evidence:

- route may be `execute_direct`
- required verification is documentation review or markdown lint if available
- no Rust build is required solely for a wording-only doc edit

Pass condition:

- evaluator reports the task can be handled as doc-only, with explicit verification proof.

### `rclean.ci_command_mapping`

When SpecRail prepares an implementation plan for `rclean`, it should map the repo's real CI commands instead of inventing generic checks.

Expected evaluator evidence:

- records `cargo fmt -- --check`
- records `cargo clippy --all-targets --all-features -- -D warnings`
- records `cargo test`
- records `cargo build --release`
- records MSRV `1.95` build/test

Pass condition:

- evaluator output lists these commands as recommended verification commands and does not replace them with unrelated tooling.

### `rclean.issue_dedupe`

When a worker considers creating a new issue for `rclean`, SpecRail should search existing local issue material first.

Expected evaluator evidence:

- detects `drafts/rclean-issues-draft-2026-05-25.md`
- detects `NOT SUBMITTED YET`
- warns that a draft exists and must be reviewed before creating a duplicate GitHub issue

Pass condition:

- evaluator returns `needs_human` for issue creation until the draft is reviewed.

## Read-only Verification Commands

These commands may be used by a human or agent to refresh the scout facts without changing `rclean`:

```sh
cd /Users/lifcc/Desktop/code/AI/tool/rclean
git status --short --branch
git remote -v
find . -maxdepth 3 -name AGENTS.md -o -name CLAUDE.md -o -name WARP.md
find . -maxdepth 3 -path './.agents/skills' -o -path './docs/specs'
find . -maxdepth 3 -path './.github/ISSUE_TEMPLATE*' -o -path './.github/PULL_REQUEST_TEMPLATE*'
test -f drafts/rclean-issues-draft-2026-05-25.md && rg -n "NOT SUBMITTED YET" drafts/rclean-issues-draft-2026-05-25.md
```

## Expected SpecRail Result

For the current scout snapshot, the smoke should not report full adoption. It should report gaps and next actions:

- missing repo-level agent context
- missing GitHub issue/PR templates
- old specs are present under `docs/specs/`
- SpecRail `specs/GH<number>/product.md` + `tech.md` path is not yet adopted
- issue draft must be reviewed before new issue creation

Expected high-level status: `needs_human`.
