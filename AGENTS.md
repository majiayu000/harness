# Harness — Project Rules

## Language

All outputs MUST be in English, including:
- Code comments and documentation
- Commit messages and PR titles/descriptions
- Prompt templates in `prompts.rs`
- Issue titles and descriptions
- CLI help text and error messages

## Build

- During implementation, prefer the smallest validation command that covers the changed surface:
  - Single crate change: `cargo check -p <crate> --all-targets`
  - Cross-crate API change: `cargo check --workspace --all-targets`
  - Targeted behavior change: run the relevant `cargo test -p <crate> <test_filter>`
- Do not run the CI-equivalent workspace check after every edit. Use it for final PR readiness or when warning-sensitive code changed.
- Before committing, run `cargo fmt --all -- --check` plus the relevant package tests. Run full workspace tests when the change affects shared behavior, persistence, workflow runtime, or agent adapters.
- Before pushing a PR, ALWAYS run `RUSTFLAGS="-Dwarnings" cargo check --workspace --all-targets` to catch CI-equivalent errors (dead code, unused imports, missing match arms)
- When adding a new enum variant, grep ALL match sites for that enum and update them — CI uses exhaustive match checks
- Run `cargo fmt --all` before every commit — CI enforces `cargo fmt --all -- --check`
- Dead code in `#[cfg(test)]` modules still triggers `-D warnings` in CI; delete unused test helpers instead of suppressing with `#[allow(dead_code)]`
- Pre-commit hook (`.githooks/pre-commit`) runs fmt + clippy + tests as a final local gate. After cloning, activate with: `git config core.hooksPath .githooks`
- Local Postgres-dependent tests require `HARNESS_DATABASE_URL`. Without it, the pre-commit hook skips the known live-Postgres lib tests and leaves full DB coverage to CI or a configured local database.

## Architecture

Harness is an agent orchestration layer. It constructs prompts and manages lifecycle — agents (Codex CLI) decide how to execute.

- ZERO `Command::new("gh")` or `Command::new("git")` calls inside harness crates — all GitHub/git interaction must be in agent prompts only
- When testing Harness product behavior for "fix issue X" or "handle PR Y", delegate to harness server (`POST /tasks`). For direct repository maintenance in this checkout, implement and verify the requested code change directly unless the user explicitly asks to exercise the Harness server flow.

## Glossary

These names overlap in everyday speech but mean different things in code. Use them precisely in commit messages, PR descriptions, and code comments.

| Term | Meaning | Location |
|---|---|---|
| **workflow runtime** | Orchestration layer that decides what should happen next; event-sourced state machine with a command outbox. | `crates/harness-workflow/src/runtime/` |
| **agent runtime** (a.k.a. `CodeAgent` / `AgentAdapter`) | Agent abstraction; receives an `AgentRequest` and returns a stream or response. | `crates/harness-core/src/agent.rs`, `crates/harness-agents/src/` |
| **`RuntimeKind`** | Label the workflow layer attaches to an agent type (`CodexExec` / `CodexJsonrpc` / `ClaudeCode` / `AnthropicApi` / `RemoteHost`). | `crates/harness-workflow/src/runtime/model.rs` |
| **task** | Legacy execution unit; submissions are being migrated to flow through the workflow runtime instead. | `crates/harness-server/src/task_runner/` |
| **runtime host** | Process instance that executes runtime jobs; can register remotely via `/api/runtime-hosts`. | `crates/harness-server/src/runtime_hosts.rs` |

There is no type literally named `AgentRuntime` in the codebase. The phrase is used informally to mean "the agent runtime layer" (i.e. `CodeAgent` and `AgentAdapter` impls). Prefer the precise names above when writing code.

## Worktree Usage

- NEVER use `isolation: "worktree"` for tasks that depend on unpushed local commits — worktrees check out from remote, missing local changes
- Before using worktree isolation, check `git log origin/main..HEAD` — if there are unpushed commits that affect the files being modified, work directly on main instead
- Worktrees are only safe for truly independent tasks on code that hasn't been locally modified

## PR Workflow

- After creating a PR, wait for Gemini code review bot before merging
- If Gemini leaves review comments, address valid feedback before merge
- If no comments or only false positives, proceed with merge

## Codex CLI Argument Order (CRITICAL)

- Codex CLI `-p` takes its prompt as the NEXT token: `Codex -p <PROMPT> [other flags...]`
- The prompt MUST immediately follow `-p`. Placing it at the end of the arg list causes "Input must be provided" errors
- Both `Codex.rs` (CodeAgent) and `claude_adapter.rs` (AgentAdapter) spawn Codex CLI — changes to CLI arg construction MUST be applied to BOTH files
- After modifying either adapter's arg construction, verify with: `cargo test --package harness-agents` (86 tests)

## Server Operation

- `harness serve` may be started from Codex when product behavior needs to be exercised, but start it with a sanitized environment so spawned agents do not inherit Codex/Claude wrapper variables.
- Prefer a standalone terminal for long-running manual dogfood sessions. For Codex-launched smoke tests, use an explicit clean environment, for example:
  `env -u Codex -u CLAUDE_CODE_ENTRYPOINT ./target/release/harness serve --transport http --port 9800 --project-root <path>`
- Before starting a server, check whether the target port is already in use. Stop only harness processes you started yourself unless the user explicitly asks otherwise.

## Dependencies

- NEVER downgrade dependency versions unless explicitly requested
- Prefer standard library over new dependencies
- Run `cargo audit` before adding security-sensitive crates

## VibeGuard Overrides (Harness-specific, from GC Learn 2026-03-19)

- RS-03 exempt: `fn main()` scope, `Mutex::lock().unwrap()`, `RwLock::{read,write}().unwrap()`
- RS-13: only flag functions returning `()` or `Result<()>` — typed returns are transformers, not action functions
- U-16 exempt: `**/prompts.rs` → 1200-line limit, `**/dispatch.rs` → 1000-line limit
- L1 exempt: new files matching `src/**/{mod,lib,main}.rs` (standard Rust module files)
- gh/git guard: AGENTS.md rule is semantic (agent prompts only); bash guard should not double-block `cargo test` subprocesses
