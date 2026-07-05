# Tech Spec

## Linked Issue

GH-1471

## Product Spec

See `specs/GH1471/product.md`.

## Current System

| Area | Files | Current behavior | Why relevant |
| --- | --- | --- | --- |
| Prompt model | `crates/harness-core/src/prompts/mod.rs` | `PromptParts` stores static/context/dynamic layers, but callers usually call `to_prompt_string()`. | The layer data exists but is discarded before execution. |
| Issue prompts | `crates/harness-core/src/prompts/issue.rs`, `contract.rs` | Issue triage, plan, implementation, and contract prompts return `PromptParts`. | These are the first practical candidates for layered execution. |
| Agent boundary | `crates/harness-core/src/agent.rs` | `AgentRequest` contains only `prompt: String`. | Agents cannot receive separate static/context/dynamic layers today. |
| Claude CLI adapter | `crates/harness-agents/src/claude.rs`, `claude_adapter.rs` | The prompt must immediately follow `-p`; no append-system-prompt args are used. | Claude CLI supports the first cache-friendly implementation path. |
| Codex adapters | `crates/harness-agents/src/codex.rs`, `codex_adapter.rs` | Prompt is sent as a positional `codex exec` prompt or JSON-RPC text input. | Local help did not expose an equivalent system-prompt cache channel, so this remains flattened. |
| Skills listing | `crates/harness-core/src/prompts/context.rs`, `crates/harness-server/src/task_executor/helpers.rs` | `build_available_skills_listing` formats every active skill description. | Prompt size grows with total skill count. |
| Prompt persistence | `crates/harness-server/src/task_executor/helpers/streaming.rs` | The redacted `req.prompt` is persisted/audited. | Layering must preserve an auditable effective prompt. |

## Proposed Design

1. Add a layered prompt payload to `AgentRequest`.
   - Keep `prompt: String` as the canonical flattened fallback and audit value.
   - Add an optional field such as `prompt_parts: Option<PromptParts>` or a
     serializable equivalent in `harness-core`.
   - Provide constructors/helpers so call sites can build an `AgentRequest`
     from `PromptParts` while automatically filling `prompt`.
2. Wire issue/task prompt call sites to preserve layers.
   - Start with `implement_from_issue`, `triage_prompt`, `plan_prompt`, and
     the issue submission contract prompts where `PromptParts` already exists.
   - Keep validation retry and continuation prompts flattened unless they have
     reliable layer boundaries.
3. Implement Claude Code layered execution.
   - Preserve the critical `-p <prompt>` argument ordering.
   - Put dynamic payload, plus any dynamic/context text that must remain user
     prompt content, immediately after `-p`.
   - Pass stable static instructions through `--append-system-prompt`.
   - Add `--exclude-dynamic-system-prompt-sections` when the layered path is
     active so Claude's dynamic default system sections do not reduce cache
     reuse.
   - Apply the same argument construction rule in both `claude.rs` and
     `claude_adapter.rs`.
4. Keep fallback behavior.
   - Codex, Anthropic API, test agents, and any request without layers should
     continue using `req.prompt`.
   - Interceptors and prompt persistence should still see the flattened
     effective prompt unless a later design adds layer-aware audit fields.
5. Cap the available-skills listing.
   - Add a deterministic cap at the listing builder or injection call site.
   - Recommended first cap: include matched skills in full via
     `build_matched_skills_section`, and limit the broad "Available Skills"
     list to a small deterministic count such as 20 active skills.
   - If the listing is truncated, include a short omitted-count line.
   - Do not cap matched full-content injection for skills that actually match.

## Data Flow

A prompt builder returns `PromptParts`. The task executor creates an
`AgentRequest` whose `prompt` is `parts.to_prompt_string()` and whose optional
layer payload carries the original static/context/dynamic strings. Shared
helpers and non-layer-aware agents continue reading `req.prompt`.

Claude Code adapters inspect the optional layer payload. If present, they send
the selected static layer via the system-prompt append argument and send the
dynamic effective prompt through `-p`. If absent, they use the current flattened
argument list.

Skill injection remains a pre-agent prompt augmentation step. The broad
available-skills listing is capped before it is appended, while matched skills
remain full content and keep `skill_used` telemetry.

## Alternatives Considered

- Replace `prompt: String` with only layered fields. Rejected because many
  adapters, interceptors, tests, prompt persistence paths, and retry builders
  depend on a flattened prompt.
- Implement cache_control through Anthropic API first. Rejected for this issue
  because the current server path primarily exercises CLI agents and the local
  Claude CLI already exposes a system-prompt append path.
- Use semantic search for the skills listing as the first fix. Deferred because
  a deterministic cap is smaller, testable, and directly addresses unbounded
  growth. Relevance search can be layered on later.

## Risks

- Misordered Claude CLI arguments can break execution. Mitigate with tests that
  assert `-p` is immediately followed by the dynamic prompt in both Claude
  adapter paths.
- Moving too much context into the system prompt could change model behavior.
  Mitigate by starting with static instructions only and keeping the flattened
  fallback available.
- Skill caps might hide useful but unmatched skills. Mitigate by preserving
  matched full-content injection and including a truncation count.
- Prompt audit could become misleading if only dynamic content is persisted.
  Mitigate by keeping `req.prompt` as the full flattened effective prompt.

## Test Plan

- [ ] `cargo test -p harness-core prompts`
- [ ] `cargo test -p harness-agents claude`
- [ ] `cargo test -p harness-agents claude_adapter`
- [ ] Focused task-executor helper tests covering skills-list cap and matched
      skill usage recording.
- [ ] `cargo check -p harness-server --all-targets`
- [ ] `python3 checks/check_workflow.py --repo . --spec-dir specs/GH1471`
- [ ] `python3 checks/check_workflow.py --repo .`
