# Harness — Project Rules

## Language

All outputs MUST be in English, including:
- Code comments and documentation
- Commit messages and PR titles/descriptions
- Prompt templates in `prompts.rs`
- Issue titles and descriptions
- CLI help text and error messages

## Build

- `cargo check` after every change
- `cargo test` before commit
- Before pushing a PR, ALWAYS run `RUSTFLAGS="-Dwarnings" cargo check --workspace --all-targets` to catch CI-equivalent errors (dead code, unused imports, missing match arms)
- When adding a new enum variant, grep ALL match sites for that enum and update them — CI uses exhaustive match checks
- Run `cargo fmt --all` before every commit — CI enforces `cargo fmt --all -- --check`
- Dead code in `#[cfg(test)]` modules still triggers `-D warnings` in CI; delete unused test helpers instead of suppressing with `#[allow(dead_code)]`

## Architecture

Harness is an agent orchestration layer. It constructs prompts and manages lifecycle — agents (Claude Code CLI) decide how to execute.

- ZERO `Command::new("gh")` or `Command::new("git")` calls inside harness crates — all GitHub/git interaction must be in agent prompts only
- When user says "fix issue X" or "handle PR Y", ALWAYS delegate to harness server (`POST /tasks`) instead of implementing directly

## PR Workflow

- After creating a PR, wait for Gemini code review bot before merging
- If Gemini leaves review comments, address valid feedback before merge
- If no comments or only false positives, proceed with merge

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
- U-16 exempt: `**/prompts.rs` → 1200-line limit, `**/dispatch.rs` → 1000-line limit
- L1 exempt: new files matching `src/**/{mod,lib,main}.rs` (standard Rust module files)
- gh/git guard: CLAUDE.md rule is semantic (agent prompts only); bash guard should not double-block `cargo test` subprocesses
