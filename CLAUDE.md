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
