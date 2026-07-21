# Harness — Project Rules

## Language

Use the user's language for conversation. Keep repository artifacts in English, including:
- Code comments and documentation
- Commit messages and PR titles/descriptions
- Prompt templates in `prompts.rs`
- Issue titles and descriptions
- CLI help text and error messages

## Build

| Changed surface | During implementation |
|---|---|
| Single crate | `cargo check -p <crate> --all-targets` |
| Cross-crate API | `cargo check --workspace --all-targets` |
| Targeted behavior | `cargo test -p <crate> <test_filter>` |

Validation workflow:

1. During implementation, run the smallest command from the table that covers the changed surface. Do not run the CI-equivalent workspace check after every edit.
2. Before committing, run `cargo fmt --all` and `cargo fmt --all -- --check`. For behavior-changing code, run the relevant package tests; run full workspace tests when the change affects shared behavior, persistence, workflow runtime, or agent adapters.
3. Before pushing a PR, run `cargo clippy --workspace --all-targets -- -D warnings` to catch CI-equivalent warnings and lints.

- When adding a new enum variant, grep ALL match sites for that enum and update them — CI uses exhaustive match checks
- Dead code in `#[cfg(test)]` modules still triggers `-D warnings` in CI; delete unused test helpers instead of suppressing with `#[allow(dead_code)]`
- Pre-commit hook (`.githooks/pre-commit`) runs fmt + staged-scope clippy as a fast commit gate. After cloning, activate with: `git config core.hooksPath .githooks`
- Pre-push hook (`.githooks/pre-push`) always runs full workspace clippy. In DB-less mode it runs database-independent workspace and `harness-workflow` lib tests under an isolated config root. With `HARNESS_DATABASE_URL`, it runs the full `harness-workflow` and `harness-server` lib suites.
- PostgreSQL-dependent `harness-workflow` and `harness-server` tests require an isolated disposable database through `HARNESS_DATABASE_URL`. Without it, pre-push defers those explicit PostgreSQL suites to CI or a configured local database.

## Architecture

Harness is an agent orchestration layer. It constructs prompts and manages lifecycle — agents (Codex CLI) decide how to execute.

- ZERO `Command::new("gh")` or `Command::new("git")` calls inside harness crates — all GitHub/git interaction must be in agent prompts only
- When testing Harness product behavior for "fix issue X" or "handle PR Y", delegate to harness server (`POST /api/workflows/runtime/submissions`). For direct repository maintenance in this checkout, implement and verify the requested code change directly unless the user explicitly asks to exercise the Harness server flow.

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

## Codex Integration

| Surface | Implementation | Invocation |
|---|---|---|
| `CodeAgent` | `crates/harness-agents/src/codex.rs` | `codex exec`; the prompt is the final positional argument |
| `AgentAdapter` | `crates/harness-agents/src/codex_adapter.rs` | `codex app-server` over stdio JSON-RPC |

- Keep changes scoped to the affected integration surface; they do not share the same CLI argument contract.
- After modifying either surface, run `cargo test --package harness-agents`.

## Server Operation

- `harness serve` may be started from Codex when product behavior needs to be exercised, but use `scripts/start-harness-codex-safe.sh` or an equivalent sanitized launcher.
- Harness strips Claude-prefixed variables before spawning child agents. Codex-prefixed wrapper variables are not stripped by the adapter spawn path, so the launcher must keep them out of the server process environment.
- Prefer a standalone terminal for long-running manual dogfood sessions when an operator needs an independently owned process.
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
