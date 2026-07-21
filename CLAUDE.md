# Harness — Project Rules

## Language

Use the user's language for conversation. Keep repository artifacts in English, including:
- Code comments and documentation
- Commit messages and PR titles/descriptions
- Prompt templates in `prompts.rs`
- Issue titles and descriptions
- CLI help text and error messages

## Build

- Use the smallest validation command that covers the changed surface during implementation.
- Run relevant tests before committing behavior-changing code.
- Before pushing a PR, ALWAYS run `cargo clippy --workspace --all-targets -- -D warnings` to catch CI-equivalent warnings and lints (dead code, unused imports, missing match arms, clippy findings)
- When adding a new enum variant, grep ALL match sites for that enum and update them — CI uses exhaustive match checks
- Run `cargo fmt --all` before every commit — CI enforces `cargo fmt --all -- --check`
- Dead code in `#[cfg(test)]` modules still triggers `-D warnings` in CI; delete unused test helpers instead of suppressing with `#[allow(dead_code)]`
- Pre-commit hook (`.githooks/pre-commit`) runs fmt + staged-scope clippy as a fast commit gate. After cloning, activate with: `git config core.hooksPath .githooks`
- Pre-push hook (`.githooks/pre-push`) always runs full workspace clippy. In DB-less mode it runs database-independent workspace and `harness-workflow` lib tests under an isolated config root. With `HARNESS_DATABASE_URL`, it runs the full `harness-workflow` and `harness-server` lib suites.
- PostgreSQL-dependent `harness-workflow` and `harness-server` tests require an isolated disposable database through `HARNESS_DATABASE_URL`; without it, pre-push defers those explicit PostgreSQL suites to CI or a configured local database.

## Local Cargo Concurrency

- Cargo cannot safely be configured to ignore its build-directory lock. If commands wait on a Cargo build lock, that is usually protecting a shared `target/` directory.
- Different projects can run Cargo concurrently as long as they do not share `CARGO_TARGET_DIR` or a global `build.target-dir`.
- Do NOT set a global shared Cargo target directory when the goal is cross-project concurrency; it makes unrelated projects contend for the same lock.
- For concurrent Cargo commands in the same repository, isolate build outputs by command:
  - `CARGO_TARGET_DIR=target/cargo-check cargo check --workspace --all-targets -j 6`
  - `CARGO_TARGET_DIR=target/cargo-test cargo test --workspace --all-targets -j 6`
  - `CARGO_TARGET_DIR=target/cargo-clippy cargo clippy --workspace --all-targets -j 6 -- -D warnings`
- On an M2 Max with 96GB RAM, keep total Cargo `-j` across concurrent jobs around 10-14 for predictable interactive performance; use lower values if other heavy builds are already running.
- If the user wants non-blocking local commands, prefer explicit shell helpers such as `cargo_fast` or `cargobg` in `~/.zshrc` rather than overriding `cargo` globally:

```bash
cargo_fast() {
  case "$1" in
    check)
      CARGO_TARGET_DIR=target/cargo-check command cargo "$@"
      ;;
    test)
      CARGO_TARGET_DIR=target/cargo-test command cargo "$@"
      ;;
    clippy)
      CARGO_TARGET_DIR=target/cargo-clippy command cargo "$@"
      ;;
    *)
      command cargo "$@"
      ;;
  esac
}

cargobg() {
  cargo_fast "$@" &
}
```

## Architecture

Harness is an agent orchestration layer. It constructs prompts and manages lifecycle — agents (Claude Code CLI) decide how to execute.

- ZERO `Command::new("gh")` or `Command::new("git")` calls inside harness crates — all GitHub/git interaction must be in agent prompts only
- When testing Harness product behavior for "fix issue X" or "handle PR Y", delegate to harness server (`POST /api/workflows/runtime/submissions`). For direct repository maintenance in this checkout, implement and verify the requested code change directly unless the user explicitly asks to exercise the Harness server flow.

## Worktree Usage

- NEVER use `isolation: "worktree"` for tasks that depend on unpushed local commits — worktrees check out from remote, missing local changes
- Before using worktree isolation, check `git log origin/main..HEAD` — if there are unpushed commits that affect the files being modified, work directly on main instead
- Worktrees are only safe for truly independent tasks on code that hasn't been locally modified

## PR Workflow

- After creating a PR, wait for Gemini code review bot before merging
- If Gemini leaves review comments, address valid feedback before merge
- If no comments or only false positives, proceed with merge
- **Squash-merge only** — enforced via GitHub ruleset (squash is the only allowed merge method; no bypass for anyone)
- **Required CI** — the `CI Result` status check must pass before merging (enforced via ruleset)
- **Do NOT modify `Cargo.toml` version in feature/fix PRs** — version bumps happen only at release time (prevents merge conflicts across parallel PRs)
- CI uses path-based change detection — only affected crate tests run on PRs

## Claude CLI Argument Order (CRITICAL)

- Claude CLI `-p` takes its prompt as the NEXT token: `claude -p <PROMPT> [other flags...]`
- The prompt MUST immediately follow `-p`. Placing it at the end of the arg list causes "Input must be provided" errors
- Both `claude.rs` (CodeAgent) and `claude_adapter.rs` (AgentAdapter) spawn Claude CLI — changes to CLI arg construction MUST be applied to BOTH files
- After modifying either adapter's arg construction, verify with: `cargo test --package harness-agents` (86 tests)

## Server Operation

- NEVER start `harness serve` from within a Claude Code session — the `CLAUDECODE` and `CLAUDE_CODE_ENTRYPOINT` env vars cause spawned agents to SIGTRAP
- Always start the server from a standalone terminal: `./target/release/harness serve --transport http --port 9800 --project-root <path>`
- If already running inside Claude Code, only stop/kill the server — let the user start it manually

## Dependencies

- NEVER downgrade dependency versions unless explicitly requested
- Prefer standard library over new dependencies
- Run `cargo audit` before adding security-sensitive crates

## VibeGuard Overrides (Harness-specific, from GC Learn 2026-03-19)

- RS-03 exempt: `fn main()` scope, `Mutex::lock().unwrap()`, `RwLock::{read,write}().unwrap()`
- RS-13: only flag functions returning `()` or `Result<()>` — typed returns are transformers, not action functions
- U-16 exempt: `**/prompts.rs` → 1200-line limit, `**/dispatch.rs` → 1000-line limit, `**/services/execution.rs` → 2600-line limit, `**/services/execution_tests.rs` → 1300-line limit, `**/task_runner/spawn.rs` → 2700-line limit, `**/task_runner/store.rs` → 2200-line limit, `**/harness-workflow/src/runtime/store.rs` → 2300-line limit, `**/harness-server/src/workflow_runtime_pr_feedback.rs` → 1900-line limit, `**/harness-server/src/http/misc_routes.rs` → 1400-line limit, `**/harness-server/src/webhook.rs` → 1000-line limit, `**/harness-cli/src/commands.rs` → 1700-line limit (legacy oversized files; pending split)
- L1 exempt: new files matching `src/**/{mod,lib,main}.rs` (standard Rust module files)
- gh/git guard: CLAUDE.md rule is semantic (agent prompts only); bash guard should not double-block `cargo test` subprocesses
