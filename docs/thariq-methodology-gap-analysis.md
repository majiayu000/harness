# Thariq's Three-Layer Methodology: Gap Analysis for Harness

> Based on three articles by Thariq (@trq212), Claude Code engineer at Anthropic:
> - [Prompt Caching Is Everything](https://www.techtwitter.com/articles/lessons-from-building-claude-code-prompt-caching-is-everything) (Feb 19, 2026)
> - [Seeing like an Agent](https://www.techtwitter.com/articles/lessons-from-building-claude-code-seeing-like-an-agent) (Feb 27, 2026)
> - [How We Use Skills](https://www.techtwitter.com/articles/lessons-from-building-claude-code-how-we-use-skills) (Mar 2026)

## Three-Layer Architecture

| Layer | Article | Core Principle |
|-------|---------|---------------|
| I. Physical | Prompt Caching | Prefix stability is the first design constraint |
| II. Perception | Seeing like an Agent | Tool set must match agent's cognitive structure |
| III. Extension | Skills | Extend capabilities without breaking Layer I or II |

## Layer I: Prompt Caching — Current State & Gaps

### What Claude Code Does
- Layout: Static system prompt & Tools → Claude.MD → Session context → Messages
- `cache_control` breakpoints at each layer boundary
- SEV alerts when cache hit rate drops
- `defer_loading: true` stubs for MCP tools (loaded on demand via ToolSearch)
- Cache-safe compaction: reuses parent conversation's full prefix
- Plan Mode: EnterPlanMode/ExitPlanMode as tools (no tool set swap)
- Model switching: only via subagents (Opus → Haiku handoff)

### Harness Current State
- Single-turn `claude -p` per execution phase — no multi-turn cache reuse
- Review loop (implement → review → fix) = N independent CLI calls, zero shared cache
- `ReasoningBudget` switches model per phase, but each phase is independent (no cache penalty)
- No `cache_control` breakpoints (not using API directly)
- No compaction (single-turn)

### Improvement Opportunities

**P3: Long-conversation mode (architecture-level)**
- Replace N × `claude -p` with single interactive session for review loops
- Would enable inter-turn cache reuse (potentially 5-10x cost reduction on review rounds)
- Requires: stdin/stdout streaming, turn management, compaction implementation
- Risk: significant architecture change; defer until API direct-call mode is built

**P1: Prompt structure separation**
- Even with CLI mode, structure prompts as `[static instructions | semi-static context | dynamic payload]`
- Makes future API migration cache-ready
- Immediate benefit: cleaner prompt composition, easier to reason about

## Layer II: Action Space — Current State & Gaps

### What Claude Code Does
- ~20 carefully curated tools, high bar to add new ones
- AskUserQuestion: dedicated tool for structured elicitation (3 iterations to get right)
- TodoWrite → Task Tool evolution: tools adapt as model capabilities grow
- Search evolution: RAG → Grep → Progressive disclosure through Skills
- Claude Code Guide: subagent instead of docs in system prompt
- Progressive disclosure: add capabilities without adding tools

### Harness Current State
- 3 coarse CapabilityProfiles: ReadOnly, Standard, Full
- Most tasks use Full profile (all tools available)
- Review agents use Full despite only needing read access
- No agent-to-user questioning mechanism (QUESTION marker)
- No progressive disclosure pattern
- Pre-packed context in prompts (issue body, PR diff injected upfront)

### Improvement Opportunities

**P0: Review agent uses ReadOnly profile**
- `review_prompt()` and `agent_review_prompt()` should use ReadOnly capability
- `periodic_review_prompt()` needs ReadOnly + Bash (for running check commands)
- Reduces risk of review agent accidentally editing files
- Simple config change in task_runner when selecting agent

**P1: Reduce pre-packed context, let agents search**
- Current: `implement_from_issue()` stuffs full issue body + labels + comments into prompt
- Better: give issue title + summary, let agent `gh issue view` and grep codebase
- Thariq: "Claude is increasingly good at building its context if given the right tools"
- Less prompt bloat, more targeted context gathering

**P2: Agent questioning mechanism**
- When task description is ambiguous, agent has no way to ask for clarification
- Add output protocol: `QUESTION: <text>` → harness pauses task, notifies user
- User responds → harness injects answer and resumes
- Mirrors AskUserQuestion but through prompt protocol

**P2: Phase-aware tool profiles**
- Extend `ReasoningBudget` to include `allowed_tools` per phase
- Planning phase: ReadOnly + search tools
- Execution phase: Full
- Validation phase: ReadOnly + Bash (for running tests)

## Layer III: Skills — Current State & Gaps

### What Claude Code Does
- Skills are **folders** (scripts, assets, data, references), not just markdown
- Description field = trigger condition for model matching, not human summary
- Gotchas section = highest-signal content, built from agent failure points
- On-demand hooks: `/careful` blocks destructive ops, `/freeze` blocks edits outside dir
- Progressive disclosure: skill references → api.md, assets/, scripts/
- Memory: skills store data (logs, JSON, SQLite) for cross-session continuity
- Measurement: PreToolUse hook logs skill usage for analytics
- 9 types: Library/API, Product Verification, Data Fetching, Business Automation,
  Code Scaffolding, Code Quality, CI/CD, Runbooks, Infrastructure Ops

### Harness Current State
- Complete 4-tier skill system (Repo > User > Admin > System)
- 10 builtin skills with content: String (pure text, no folder structure)
- `match_prompt()` and `match_context()` matching available
- Usage tracking (count, last_used) with sidecar .usage.json files
- **NOT wired to agent prompts** — agents cannot see or trigger skills
- No gotchas mechanism
- No on-demand hooks
- No folder-based skills (scripts, assets, references)
- Descriptions not optimized as trigger conditions

### Improvement Opportunities

**P0: Wire skills into prompt construction**
- When building agent prompt, call `skills.match_prompt(&task.prompt)`
- Inject matched skill stubs (name + description) into prompt footer
- Add instruction: "Available skills — if relevant, request skill content by name"
- Agent outputs `SKILL: <name>` → harness injects full content into next turn
- For single-turn `claude -p`: inject full matched skill content directly

**P1: Rewrite skill descriptions as trigger conditions**
- Current: human-readable summaries
- Needed: "Use when: user asks to review code, CI fails, PR has comments"
- Each builtin skill description should answer "when should the agent use this?"

**P1: Skill folder structure support**
- Extend `Skill` struct: `pub assets_dir: Option<PathBuf>`
- Allow skills to have `references/`, `scripts/`, `assets/` subdirectories
- Agent can discover and read files within the skill folder
- Enables complex skills (verification scripts, template files, helper libraries)

**P2: Gotchas mechanism**
- Add `gotchas: Vec<String>` to Skill struct
- Populate from agent failure analysis (manual initially, automated later)
- Include gotchas in skill content when injected into prompts
- This is the "cross-session education" feedback loop

**P2: On-demand hooks**
- Skills can register PreToolUse / PostToolUse hooks active only during skill execution
- Example: `review` skill blocks Write/Edit tools
- Example: `deploy` skill requires confirmation before Bash commands with `push`/`deploy`

**P3: Skill usage metrics**
- Hook into prompt construction to log when skills are matched and used
- Track: match rate, trigger rate, success rate per skill
- Identify under-triggering or over-triggering skills

## Priority Summary

| Priority | Item | Layer | Effort | Impact |
|----------|------|-------|--------|--------|
| **P0** | Wire skills into prompt construction | III | Small | Agent can discover and use skills |
| **P0** | Review agent uses ReadOnly profile | II | Trivial | Reduce review agent risk |
| **P1** | Prompt structure separation | I | Medium | Cache-ready architecture |
| **P1** | Skill descriptions as trigger conditions | III | Small | Better auto-matching |
| **P1** | Skill folder structure support | III | Medium | Enable complex skills |
| **P1** | Reduce pre-packed context | II | Medium | Less bloat, smarter agents |
| **P2** | Agent questioning mechanism | II | Medium | Handle ambiguous tasks |
| **P2** | Phase-aware tool profiles | II | Medium | Right tools per phase |
| **P2** | Gotchas mechanism | III | Small | Cross-session learning |
| **P2** | On-demand hooks for skills | III | Large | Skill-level safety policies |
| **P3** | Long-conversation mode | I | Large | Inter-turn cache reuse |
| **P3** | Skill usage metrics | III | Medium | Data-driven optimization |

## Implementation Order

Recommended batches for task submission:

### Batch 1 (P0 — immediate value)
1. Wire skills into prompt construction
2. Review agent capability profile fix

### Batch 2 (P1 — structural improvements)
3. Prompt structure: separate static/dynamic in AgentRequest
4. Rewrite builtin skill descriptions as trigger conditions
5. Skill folder structure support

### Batch 3 (P2 — advanced features)
6. Agent questioning protocol (QUESTION marker)
7. Phase-aware tool profiles
8. Gotchas field in Skill struct

### Batch 4 (P3 — architecture evolution)
9. Long-conversation mode (API direct call)
10. Skill on-demand hooks
11. Skill usage metrics
