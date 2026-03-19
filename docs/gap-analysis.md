# Harness vs OpenAI Harness Engineering — Gap Analysis

> Date: 2026-03-13 (updated)
> Scope: 8-dimension coverage comparison — updated after 27 architectural PRs merged

## Summary

**Overall coverage: ~56%** — Core JSON-RPC routing, thread/task persistence, and observability infrastructure are in place. Significant gaps remain in runtime rule enforcement, multi-agent parallel dispatch, improvement cycle tracking, and external feedback ingestion.

## Coverage Table

| Dimension | Coverage | Status | Notes |
|-----------|----------|--------|-------|
| Golden Principles | 70% | Rules scannable; pre-turn interceptor wired | Missing: runtime hook blocking, TurnSteer, auto-PR on GC violation |
| App Server (Thread/Turn/Item) | 55% | Core methods routed, Thread+Task persisted to SQLite | Missing: real streaming, approval flow, full WS handshake |
| GC Agent | 60% | Signal detection + draft lifecycle + adopt pipeline | Missing: auto PR creation, artifact parsing, incremental scanning |
| Taste Invariants | 65% | 83 rules deployed, QualityGrader 4-axis scoring | Missing: auto-fix, guard script library, real-time hook integration |
| Improvement Cycles | 35% | ExecPlan Markdown serialization, MEMORY.md via remem | Missing: ExecPlan DB persistence, skill usage stats, auto-trigger |
| Feedback Loops | 40% | EventStore (JSONL), QualityGrader, GC signal detection | Missing: EventLog/EventQuery routing, external signals, telemetry export |
| Multi-Agent Coordination | 25% | AgentRegistry multi-agent routing, async task spawn | Missing: true parallel dispatch, TaskClassification use, agent isolation |
| Skill System | 50% | 4-layer discovery, CRUD CLI, trigger_patterns field | Missing: auto-injection, persistence, routing, trigger_patterns parsing |

## Dimension Details

### 1. Golden Principles — Architecture Constraint Layer (70%)

**What we have:**
- `rules/golden-principles.md` defines 5 GP rules (GP-01 through GP-05) aligned with OpenAI's original: executable artifacts, diagnostics-first, mechanical execution, observable operations, maintain the map
- `harness-rules/src/engine.rs` embeds golden-principles.md via `include_str!` at compile time, scannable through `rule check`
- Rules parsed by `## ID: Title (severity)` format; SEC-01, GP-01 etc. all recognized

**What's missing:**
1. **Runtime enforcement**: Rules can only static-scan (`RuleEngine::scan` calls external bash guard scripts). No hook connection points — no pre-tool-use/post-tool-use event-driven blocking. Router returns `METHOD_NOT_FOUND` for all non-Initialize/Thread/Turn methods
2. **TurnSteer not implemented**: GP principle "correct while executing" needs `TurnSteer`, which is declared but returns METHOD_NOT_FOUND
3. **GC doesn't auto-create PRs**: OpenAI's GC opens PRs on violation discovery; ours only produces Draft files (JSON), requiring manual `harness gc adopt`

**Effort to close**: ~3-5 days — wire GcAgent's adopt flow to ClaudeCodeAgent's PR creation + implement hook injection points

### 2. App Server — Thread/Turn/Item + JSON-RPC Protocol (55%)

**What we have:**
- `harness-core/src/types.rs` defines complete Thread/Turn/Item data structures, semantically aligned with OpenAI Codex App Server
- `harness-protocol` implements standard JSON-RPC 2.0: `RpcRequest`, `RpcResponse`, `RpcError` with proper error codes
- `Method` enum declares full method table (~25 methods covering Thread/Turn/GC/Skill/Rule/ExecPlan/Observe)
- Both stdio and HTTP transports implemented (`serve_stdio`, `serve_http`)
- `Notification` enum defines server push events (TurnStarted/ItemStarted/ItemCompleted/TurnCompleted/TokenUsageUpdated/ThreadStatusChanged)
- ThreadManager implements start/get/list/delete/start_turn/complete_turn/cancel_turn/add_item

**What's missing:**
1. **Most methods return METHOD_NOT_FOUND**: `router.rs` only implements Initialize/ThreadStart/ThreadList/ThreadDelete/TurnStart; remaining 20+ methods all error
2. **No real streaming**: `execute_stream` in ClaudeCodeAgent/CodexAgent is pseudo-stream (full output then single Done via channel), not true delta streaming
3. **Thread persistence missing**: ThreadManager is pure in-memory DashMap, threads lost on restart (Tasks have SQLite persistence, Threads don't)
4. **Approval Flow not implemented**: ApprovalRequest Item type defined but no request/response workflow
5. **WebSocket transport not implemented**: `Transport::WebSocket` enum exists, but `serve_http` uses only axum HTTP with no WS upgrade
6. **Initialization handshake incomplete**: OpenAI spec requires bidirectional handshake (`initialize` → `initialized`), we only handle `initialize`

**Effort to close**: ~2-3 weeks — core gaps are streaming (needs SSE or WebSocket + real subprocess stdout streaming) and Thread persistence

### 3. GC Agent — Auto-scan Codebase, Create PRs for Violations (60%)

**What we have:**
- `harness-gc` crate fully structured: GcAgent + SignalDetector + DraftStore + Remediation
- SignalDetector implements 5 signal types: RepeatedWarn, ChronicBlock, HotFiles, SlowSessions, WarnEscalation + LinterViolations
- GcAgent.run() flow: detect signals → call agent to generate fix → save Draft, with budget limit (per_signal_usd)
- Draft state machine: Pending → Adopted / Rejected / Expired
- CLI complete: `harness gc run/status/drafts/adopt/reject`
- Scheduling: `scripts/gc-scheduled.sh` + launchd timer

**What's missing:**
1. **No auto PR creation**: `gc adopt` only `fs::write` to disk, no git commit + push + create PR automation. OpenAI's GC closes the full loop: violation → fix → PR
2. **Artifact parsing is stub**: `parse_artifacts()` stores entire agent output as single markdown, doesn't actually parse code diffs/file contents
3. **No real guard scripts integrated**: `RuleEngine::scan` needs registered Guards (bash scripts), but GcAgent.run calls with empty violations
4. **No incremental scanning**: Every run reads all events, no checkpoint mechanism
5. **No draft expiration**: DraftStatus::Expired exists but no cleanup logic

**Effort to close**: ~1 week — adopt path → PR creation + artifact parsing

### 4. Taste Invariants — Code Style/Quality Enforcement (65%)

**What we have:**
- 83 rules deployed to `~/.claude/rules/vibeguard/` covering Rust/TypeScript/Python/Go + common SEC/U rules
- `engine.rs` `load_builtin()` embeds 7 rule files
- YAML frontmatter `paths:` filtering routes rules by file extension
- Rule IDs categorized by prefix (SEC-/GP-/RS-/GO-/TS-/PY-/U-)
- QualityGrader implements 4-axis scoring: Security×0.4 + Stability×0.3 + Coverage×0.2 + Performance×0.1
- Grade A/B/C/D maps to GC trigger frequency (Grade D = hourly GC)

**What's missing:**
1. **No auto-fix**: Rule struct has `fix_pattern` field but always None — no lint→autofix loop
2. **Guard script library empty**: Rules define "what to check" but Guard scripts (bash/regex detection) don't exist in the codebase; `RuleEngine::scan` needs `register_guard` first
3. **No real-time hook integration**: Taste invariants should fire on every AI file write; no pre/post tool-use hook connects RuleEngine
4. **QualityGrader has no CI output**: Quality score computed but not exported (no metrics, no webhook)

**Effort to close**: ~1 week — guard script development + hook injection

### 5. Improvement Cycles — Capability Evolution Tracking (35%)

**What we have:**
- ExecPlan (`harness-exec`) implements full plan management: purpose/milestones/steps/decision_log/surprises/validation
- ExecPlan supports Markdown serialization/deserialization (cross-session resume)
- `harness plan init/status` CLI commands
- MEMORY.md system (via remem MCP) tracks project decision history

**What's missing:**
1. **ExecPlan not persisted to DB**: Only saved as markdown files in CWD, can't list all plans via API or filter by project
2. **No capability evolution tracking**: OpenAI tracks "which skill used how many times, with what effect"; our Skill system has no usage stats
3. **No document freshness checking**: No mechanism to detect stale rules/skills (OpenAI has "freshness score")
4. **ExecPlanUpdate not implemented**: Protocol method exists but router returns METHOD_NOT_FOUND
5. **No automatic improvement triggers**: Can't auto-trigger GC → fix → ExecPlan update based on quality scores

**Effort to close**: ~2 weeks — ExecPlan SQLite storage + skill usage stats + auto-trigger logic

### 6. Feedback Loops — Production Signal Collection (40%)

**What we have:**
- `harness-observe` crate: EventStore (JSONL), QualityGrader, statistics aggregators
- Event structure: ts/session_id/hook/tool/decision/reason/detail/duration_ms
- EventFilters: filter by session/hook/decision/time range
- `EventLog` and `EventQuery` methods declared in protocol
- GC SignalDetector consumes events (observe → signal → gc → fix embryonic loop)

**What's missing:**
1. **EventLog/EventQuery not routed**: Protocol methods exist but router returns METHOD_NOT_FOUND
2. **No production signal ingestion**: Only internal hook events; no GitHub CI failure, external review score, or user complaint signals
3. **PR review feedback not structured**: PR review loop's LGTM/FIXED results stored only in task rounds JSON, not flowing back to EventStore as aggregatable feedback signals
4. **No telemetry export**: Observability statistics are not exported to Prometheus/OpenTelemetry/external sinks
5. **EventStore uses JSONL files**: Comment says "SQLite upgrade path available" but not implemented; full-file reads on large event volumes

**Effort to close**: ~1 week — router wiring + PR feedback structuring + SQLite migration

### 7. Multi-Agent Coordination — Parallel Agent Dispatch (25%)

**What we have:**
- AgentRegistry supports multiple agents (claude/codex/anthropic-api), `dispatch()` routes by TaskComplexity: Complex/Critical → claude, others → default
- `TaskComplexity` enum (Simple/Medium/Complex/Critical)
- ReasoningBudget defines 3-phase budget: planning(xhigh) → execution(high) → validation(xhigh), mapped to models (opus/sonnet/haiku)
- `spawn_task` uses `tokio::spawn` for non-blocking async tasks, concurrent execution supported

**What's missing:**
1. **No true parallel dispatch**: `spawn_task` is async but each task uses one agent serially for all turns — no "multiple agents working on different subtasks simultaneously"
2. **TaskClassification unused**: Type defined but no code path creates or uses TaskClassification to drive `dispatch()`
3. **ReasoningBudget not integrated**: Budget defines per-phase model selection, but `ClaudeCodeAgent.execute()` always uses `req.model` or `default_model`, never switches by planning/execution/validation phase
4. **No agent isolation**: Stripe Minions-style tool subset (GC agent: Read/Grep/Glob only; implement agent: Write/Execute allowed) is hardcoded in GcAgent but not systemized as an agent capability constraint layer
5. **Codex agent not registered in CLI**: commands.rs only registers claude; codex/anthropic-api agent code exists but entry points not wired

**Effort to close**: ~2-3 weeks — task decomposition logic + parallel spawn + agent isolation mechanism

### 8. Skill System — Team Knowledge Accumulation (50%)

**What we have:**
- `harness-skills` crate with 4-layer discovery: repo(.harness/skills/) → user(~/.harness/skills/) → admin(/etc/harness/) → system
- SkillStore supports create/get/delete/list/search (by name/description keywords)
- Skill has trigger_patterns field (glob matching), `match_context()` by file path or language
- Deduplication: same-name skills keep highest priority by location (repo > user > admin > system)
- CLI: `harness skill list/create/delete`
- Protocol: `SkillCreate/SkillList/SkillGet/SkillDelete` methods declared

**What's missing:**
1. **Skills not auto-injected into agent context**: `AgentRequest.context: Vec<ContextItem>` has `ContextItem::Skill` variant, but no code queries SkillStore and injects matching skills when building AgentRequest
2. **SkillStore not persisted**: `create()` adds to in-memory Vec, lost on process exit (no file write, no DB)
3. **Skill methods not routed**: SkillCreate/SkillList all return METHOD_NOT_FOUND
4. **trigger_patterns always empty**: load_from_dir loads skills with `trigger_patterns: Vec::new()`, so match_context always returns empty
5. **No skill versioning**: version field fixed at "1.0.0", no change tracking
6. **GC-generated skills not registered**: GcAgent Skill artifacts written to `.harness/drafts/`, adopt `fs::write` to target_path, not registered in SkillStore

**Effort to close**: ~1 week — persistence + auto-injection + trigger_patterns parsing

## Highest-ROI Improvements (by effort/impact ratio)

| Priority | Action | Effort | Impact |
|----------|--------|--------|--------|
| 1 | **Wire remaining router methods** — EventLog/GcRun/SkillList etc. to make existing code usable | ~1 week | +10% coverage |
| 2 | **GC adopt → auto PR** — Connect ClaudeCodeAgent to complete GC loop | ~3 days | +5% coverage |
| 3 | **Skill persistence + auto-injection** — Make skill system deliver real value | ~3 days | +5% coverage |
| 4 | **Thread persistence to SQLite** — Reuse TaskStore pattern | ~3 days | +5% coverage |

These 4 items total ~2 weeks and would raise coverage from **50% to ~70%**. The remaining Multi-Agent parallel dispatch and real-time streaming are architecture-level changes requiring 3+ weeks.

## Development Log (2026-03-02 ~ 2026-03-03)

### Completed Work

1. **SQLite Task Persistence** (PR #11)
   - TaskDb module with DashMap cache + SQLite backend
   - Auto-recovery on server restart via `open()` → load all tasks to cache
   - HARNESS_DB env var, default `~/.local/share/harness/tasks.db`

2. **Review Loop Convergence Strategy** (PR #12)
   - Round-aware severity filtering: Round 2 = critical/high/medium; Round 3+ = critical/high only
   - Conditional `/gemini review`: only on final round to prevent comment inflation
   - Per-turn timeout: `tokio::time::timeout` wrapping agent.execute(), default 600s
   - PR_URL fallback: `agent_output.or(req.pr)` when agent omits output

3. **Dependency Protection** (PR #13)
   - CLAUDE.md rule: "NEVER downgrade dependency versions"
   - Prompt constraint: review_prompt includes explicit no-downgrade instruction
   - Validated: PR #15 Gemini suggested serde_yaml downgrade, agent correctly ignored it

4. **Built-in Rules + Unit Tests** (PR #14)
   - RuleEngine.load_builtin() embeds 7 rule files via include_str!
   - YAML frontmatter parsing with serde_yaml
   - 30+ unit tests across 8 crates (from zero test coverage)

5. **Panic Fix + GC Signal Detection** (PR #15)
   - Removed 4 unwrap/expect panic paths
   - GC SignalDetector with configurable thresholds
   - QualityGrader scoring and grade mapping

6. **End-to-End Validation** (Issue #16 → PR #17)
   - Task created via HTTP API → 3-turn convergence → LGTM
   - Total time: 6.5 minutes
   - Validates full chain: API → cache+DB → agent execution → state update → restart recovery

### Problems Encountered and Solutions

| # | Problem | Root Cause | Solution |
|---|---------|-----------|----------|
| 1 | Agent skips review loop, marks done immediately | `check_existing_pr` prompt missing PR_URL output instruction | Added "Always print PR_URL=..." to prompt |
| 2 | Gemini comments inflated from 10→23 | Every round triggered `/gemini review` unconditionally | Conditional trigger: only on final round |
| 3 | Agent skipped medium severity comments | Gemini's two medium suggestions contradicted each other | Round-aware filtering: early rounds fix all, later rounds focus on critical |
| 4 | Agent downgraded serde_yaml 0.9→0.8 | Gemini marked "security-critical" suggesting pure-Rust YAML parser | Prompt constraint "NEVER downgrade" + manual revert |
| 5 | PR_URL parsing failure when agent omits output | Task runner only parsed agent stdout for PR_URL | Fallback to `req.pr` parameter |
| 6 | Agent timeout with no protection | Long-running agent execution blocks task runner | `tokio::time::timeout` per-turn, default 600s |
| 7 | Rebase conflicts across 5 PRs | Concurrent PRs modifying shared files | Sequential rebase resolution per PR |
| 8 | Test assertion failure after prompt change | New prompt contained "issues" keyword matching `!p.contains("issue")` | Narrowed assertion to `!p.contains("issue #")` |

### Validation Results

| PR | Comments Before | Comments After | Inflation | Status |
|----|----------------|----------------|-----------|--------|
| #11 | 26 | 26 | 0 (previously 10→23) | MERGED |
| #12 | 7 | 9 | +2 (controlled) | MERGED |
| #13 | 3 | 3 | 0 | MERGED |
| #14 | - | - | - | MERGED |
| #15 | 5 | 9 | +4 (controlled, ignored downgrade) | MERGED |

### Architecture Decisions

1. **Harness = Agent Orchestration Layer** — Zero `Command::new("gh"/"git")` in Rust code; all GitHub/git interaction delegated to agent prompts
2. **Hybrid Storage** — DashMap (high-frequency queries) + SQLite (crash recovery), synchronized on insert/persist
3. **4-Layer Review Protection** — Severity filtering + conditional Gemini trigger + timeout + architecture constraints
4. **Trust Model** — Runner doesn't add independent verification; convergence achieved through prompt optimization ensuring agent's autonomous LGTM decisions are correct
