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

## Architecture

Harness is an agent orchestration layer. It constructs prompts and manages lifecycle — agents (Claude Code CLI) decide how to execute.

- ZERO `Command::new("gh")` or `Command::new("git")` calls inside harness crates — all GitHub/git interaction must be in agent prompts only
- When user says "fix issue X" or "handle PR Y", ALWAYS delegate to harness server (`POST /tasks`) instead of implementing directly

## Dependencies

- NEVER downgrade dependency versions unless explicitly requested
- Prefer standard library over new dependencies
- Run `cargo audit` before adding security-sensitive crates
