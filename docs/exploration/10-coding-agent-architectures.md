# Coding Agent Architectures: A Comprehensive Survey

A survey of 20+ coding agents covering architecture, context management, editing approaches, and performance.

---

## Part 1: Single-Agent Systems

### OpenAI Codex

**Architecture**: Stateless ReAct loop

```
User Task → [Sandbox VM]
             ├── ReAct Loop:
             │   Think → Act (tool call) → Observe (result) → Think → ...
             ├── Tools: file read/write, bash, web fetch
             └── Prefix caching for repeated context
```

**Key characteristics**:
- Each task runs in an isolated **Landlock sandbox** (Linux security module)
- No persistent state between tasks
- **Prefix caching**: common context (system prompt, repo structure) is cached across turns, reducing latency and cost
- Internet access can be enabled/disabled per task
- Model: GPT codex-1 (specialized for coding)

**Codebase understanding**: Relies on file listing + grep-like search. No embeddings or semantic index.

**File editing**: Applies edits as unified diffs. Multiple edit formats supported.

**Testing**: Runs tests in sandbox, observes output, iterates. No special test framework integration.

**SWE-bench Verified**: ~69%

---

### Claude Code

**Architecture**: "nO" loop (single-threaded observation loop)

```
User Input → Agent Loop:
              ├── Generate response (may include tool calls)
              ├── Execute tool calls sequentially
              ├── Append results to conversation
              └── Repeat until response has no tool calls
```

**Key characteristics**:
- **Single-threaded**: one conversation, sequential tool execution
- **No embeddings**: uses glob + grep for codebase navigation
- **Depth-limited subagents**: can spawn sub-agents for parallel work, but sub-agents cannot spawn their own
- **Tool Search Tool**: meta-tool that finds relevant tools, reducing context from tool definitions by 85%
- Tools: Read, Write, Edit, Glob, Grep, Bash, WebSearch, subagent

**Codebase understanding**: Glob patterns for file discovery, grep for content search. No vector DB, no semantic search. This works because code structure is hierarchical (directory → file → function) not semantic.

**File editing**: Exact string replacement via Edit tool. Requires reading the file first to get exact match strings. Falls back to full file Write for major changes.

**Context management**:
- Extended thinking for complex reasoning
- Compaction (summarization) when context grows large
- CLAUDE.md files for persistent project knowledge
- Sub-agents for context isolation on parallel subtasks

**Testing**: Runs test commands via Bash tool. No special integration — treats tests as any other command.

**SWE-bench Verified**: ~72%

---

### Aider

**Architecture**: Architect/Editor dual-model pattern

```
User Request → Architect (strong model):
                ├── Analyze repository structure
                ├── Decide which files to edit
                └── Describe changes in natural language
                     │
                     ▼
               Editor (fast model):
                ├── Receive change descriptions
                ├── Generate actual code edits
                └── Apply to files
```

**Key characteristics**:
- **tree-sitter + PageRank repo map**: parses codebase AST, ranks files by connectivity (PageRank on import/call graph), provides most relevant files to architect
- **Dual model**: architect (expensive, high quality) for planning, editor (cheap, fast) for mechanical code changes
- **Edit formats**: supports unified diff, whole file, and search/replace formats
- CLI-based, works with any git repo

**Codebase understanding**: The repo map is Aider's key innovation:
1. Parse all files with tree-sitter → extract symbols (functions, classes, imports)
2. Build call/import graph between symbols
3. Run PageRank to identify most connected (important) files
4. Provide top-ranked files + their symbols as context

This is more sophisticated than grep-based search but lighter than full embeddings.

**File editing**: Search/replace blocks with fuzzy matching. The editor model generates search/replace pairs:
```
<<<<<<< SEARCH
def old_function():
    return None
=======
def old_function():
    return 42
>>>>>>> REPLACE
```

**Testing**: Runs tests, feeds failures back to architect for analysis. Supports auto-fix loops.

**SWE-bench Verified**: ~48% (with Opus)

---

## Part 2: Multi-Agent Systems

### Cursor

**Architecture**: Planner/Worker/Apply model with parallel execution

```
User Request → Planner Agent:
                ├── Analyze codebase (uses indexed embeddings)
                ├── Create plan with specific file targets
                └── Spawn workers
                     │
        ┌────────────┼────────────┐────────────┐
        ▼            ▼            ▼            ▼
   Worker 1     Worker 2     Worker 3     Worker N
   (file A)     (file B)     (file C)     (file D)
        │            │            │            │
        ▼            ▼            ▼            ▼
   Apply 1      Apply 2      Apply 3      Apply N
   (merge)      (merge)      (merge)      (merge)
```

**Key characteristics**:
- **Up to 8 parallel worker agents** for large changes
- **Embedding-based codebase index**: full semantic search over codebase
- **Apply model**: specialized small model for merging edits into existing code (fast, cheap)
- **Background Agents**: can run tasks asynchronously in cloud VMs, accessible from multiple devices
- Tab completion, inline edits, and agent mode as different interaction levels

**Codebase understanding**: Embedding-based index of all files. Chunks files, embeds them, stores in local vector DB. Combined with tree-sitter for symbol-level navigation.

**File editing**: Workers generate diffs, Apply model merges them into existing code. The Apply model is a small, specialized model trained specifically for this merge task — much cheaper than using the main model.

**Testing**: Runs tests, feeds failures back to planner for re-planning.

**SWE-bench Verified**: ~65%

---

### Devin (Cognition)

**Architecture**: Planner/Coder/Critic/Browser with fleet execution

```
User Request → Planner:
                ├── Decompose into subtasks
                ├── Manage execution timeline
                └── Coordinate agents
                     │
        ┌────────────┼────────────┐
        ▼            ▼            ▼
     Coder       Browser       Critic
     Agent        Agent        Agent
     (edits)     (researches)  (reviews)
```

**Key characteristics**:
- **Persistent VM**: each Devin session runs in a long-lived VM with full desktop environment
- **Browser agent**: can navigate web for documentation, Stack Overflow, API docs
- **Critic agent**: reviews code changes before committing
- **Fleet execution**: multiple Devin instances can work on different tasks in parallel
- **Slack integration**: communicates progress and asks questions via Slack

**Codebase understanding**: Full VM access with IDE-like tools. Uses shell commands, file search, and browser for context gathering.

**File editing**: Direct file editing in VM. Full editor capabilities.

**Testing**: Runs tests in VM, observes output, iterates with Coder agent.

**SWE-bench Verified**: ~55%

---

### Cosine Genie

**Architecture**: Custom model with self-improvement training

**Key characteristics**:
- **Custom-trained model**: not based on general-purpose LLM API calls. Cosine trained their own model specifically for coding agent tasks.
- **Self-improvement**: model is trained on its own successful task completions, creating a flywheel
- **Competitive SWE-bench scores** with much lower inference cost due to specialized model

Limited public architectural details available.

---

### Augment Code

**Architecture**: Coordinator/Specialists/Verifier

```
User Request → Coordinator:
                ├── Analyze request scope
                ├── Query Context Engine for relevant code
                └── Assign to specialists
                     │
        ┌────────────┼────────────┐
        ▼            ▼            ▼
   Code Writer   Test Writer   Doc Writer
   Specialist    Specialist    Specialist
        │            │            │
        └────────────┼────────────┘
                     ▼
               Verifier Agent
               (cross-checks all changes)
```

**Key characteristics**:
- **Context Engine**: proprietary system that indexes and retrieves relevant code context across large codebases. Handles monorepos with millions of lines.
- **Specialist agents**: dedicated agents for code, tests, and documentation
- **Verifier**: cross-checks changes from all specialists for consistency
- **Enterprise focus**: designed for large teams and codebases

---

## Part 3: Hybrid Systems

### Windsurf (Codeium)

**Architecture**: Flow-based with persistent context

**Key characteristics**:
- **Flows**: persistent context threads that maintain state across interactions
- **Cascade**: AI engine that combines code generation, codebase understanding, and command execution
- **Deep codebase awareness**: indexes and understands large codebases
- **IDE-first**: built as a full IDE (fork of VS Code), not a plugin

**Context management**: Flows maintain conversation context + relevant code context across sessions. This is the key differentiator — unlike Claude Code (fresh context each session) or Cursor (session-based), Windsurf maintains persistent context.

---

### Amazon Q Developer

**Architecture**: Code transformation agents

**Key characteristics**:
- **Code transformation**: specialized agents for language upgrades (Java 8 → 17), framework migrations (Spring Boot 2 → 3)
- **Security scanning**: integrated vulnerability detection and automated remediation
- **AWS-integrated**: deep integration with AWS services (Lambda, ECS, CloudFormation)
- **/dev agent**: autonomous coding agent for feature implementation

---

### GitHub Copilot

**Architecture**: Agent Mode + Coding Agent

```
Agent Mode (interactive):
  User ↔ Agent (in VS Code, iterative)

Coding Agent (autonomous):
  Issue → Cloud VM → Branch + PR
```

**Key characteristics**:
- **Agent Mode**: interactive agent within VS Code/JetBrains. Iterative conversation with tool use.
- **Coding Agent**: fully autonomous. Triggered by GitHub issue assignment. Runs in cloud VM, creates branch, makes changes, opens PR.
- **MCP support**: can connect to MCP servers for additional tools
- **GitHub-native**: deep integration with Issues, PRs, Actions

---

### Google Jules

**Architecture**: Cloud VM per task

**Key characteristics**:
- Each task runs in an **isolated cloud VM**
- Full development environment with terminal, editor, browser
- Focused on multi-file code changes
- Integrated with Google's AI infrastructure (Gemini models)
- Creates plans before executing, allows human review of plans

---

## Part 4: Open-Source Systems

### SWE-Agent

**Architecture**: Agent Computer Interface (ACI)

```
Issue → ACI Shell:
         ├── Custom commands: open, search, edit, scroll
         ├── Linter integration (immediate feedback)
         └── Constrained action space
```

**Key characteristics**:
- **ACI (Agent Computer Interface)**: custom shell interface designed for LLM agents, not humans. Simplified commands, structured output.
- **mini-SWE-agent**: demonstrates that the core logic is only ~100 lines of Python, achieving >74% on HumanEval
- **Constrained action space**: limits what the agent can do at each step, reducing errors
- **Linter feedback**: immediate syntax checking after each edit

**SWE-bench Lite**: ~40%

---

### OpenHands (formerly OpenDevin)

**Architecture**: Event-stream-based

```
Events: [UserMessage, AgentAction, Observation, AgentAction, Observation, ...]

Agent observes event stream → decides next action → action produces observation → repeat
```

**Key characteristics**:
- **Event-stream**: all interactions are events in a stream. Agent, tools, and user all produce events.
- **Docker sandbox**: each session runs in isolated Docker container
- **Rich tool set**: browser, terminal, file editor, code execution
- **OpenHands-Versa**: the generalist variant that beat Magentic-One on GAIA (64.24% vs 46.06%)

**SWE-bench Verified**: competitive (varies by model)

---

### Agentless

**Architecture**: 3-phase pipeline (no agent loop)

```
Phase 1: Localization → Identify files and functions
Phase 2: Repair → Generate candidate patches
Phase 3: Validation → Test and select best patch
```

**Key characteristics**:
- **No agent**: no tool use, no iterative loop. Pure pipeline.
- **Multiple candidates**: generates several patches, selects best via testing
- **$0.34 per bug** average cost (later optimized from original $0.70)
- Proves agent loop is optional for well-defined tasks

**SWE-bench Lite**: ~32%

---

### Moatless Tools

**Architecture**: FSM (Finite State Machine)

```
States: Search → Identify → Plan → Edit → Verify → Complete
Transitions: based on agent decisions and tool results
```

**Key characteristics**:
- **$0.01 per issue** (extremely cheap)
- **FSM-based**: explicit states and transitions, not free-form agent loop
- **Focused search**: efficient code localization using structured search
- Proves that structure reduces cost dramatically

---

### Open SWE (Patched.codes)

**Architecture**: Planner/Agent/Reviewer

```
Planner → creates step-by-step plan
Agent → executes each step
Reviewer → validates results, requests changes
```

3-agent pipeline with explicit review.

---

### Cline

**Architecture**: Single-agent with rich tool use

**Key characteristics**:
- VS Code extension
- Rich tool integration (terminal, file editing, browser)
- Supports multiple LLM providers
- Open-source, highly configurable
- MCP support for additional tools

---

### Goose (Block)

**Architecture**: MCP-native, written in Rust

```
User → Goose Agent → MCP Servers (tools)
                    ├── developer (file/terminal)
                    ├── github
                    ├── jira
                    └── custom MCP servers
```

**Key characteristics**:
- **Written in Rust** — fast, low resource usage
- **MCP-native**: all tools are MCP servers, no built-in tools
- **Extensible**: add capabilities by connecting MCP servers
- **CLI-first**: terminal interface with rich formatting
- **Open-source** (Block/Square)

---

### OpenDev (OpenDevin variant)

**Architecture**: 6-phase ReAct with 5-model routing

```
Phase 1: Understand (Opus) → deep comprehension
Phase 2: Plan (Opus) → strategy
Phase 3: Implement (Sonnet) → code writing
Phase 4: Test (Haiku) → test execution
Phase 5: Debug (Sonnet) → fix failures
Phase 6: Review (Opus) → final check
```

**Key characteristics**:
- **5-model routing**: uses different models for different phases based on required capability and cost
- **6-phase structure**: fixed phases but adaptive within each phase
- Cost-optimized: cheap models for mechanical tasks, expensive for reasoning

---

## Part 5: Comparison Tables

### Codebase Understanding Approaches

| System | Approach | Tradeoffs |
|--------|----------|-----------|
| Claude Code | glob + grep | Simple, fast, no index needed. Misses semantic relationships |
| Cursor | Embeddings + vector DB | Rich semantic search. Requires indexing, storage, maintenance |
| Aider | tree-sitter + PageRank | Structural understanding. Language-specific parsers needed |
| Codex | File listing + search | Simple, stateless. Limited codebase awareness |
| Devin | Full VM + shell tools | Maximum capability. Heavy resource usage |
| Windsurf | Persistent indexed context | Rich, maintained over time. Complex state management |
| SWE-Agent | Custom ACI commands | Optimized for agent use. Limited to ACI capabilities |
| Goose | MCP-based tools | Extensible. Quality depends on MCP server implementations |

### Context Management Strategies

| System | Strategy | Max Effective Context |
|--------|----------|----------------------|
| Claude Code | Compaction + sub-agents + CLAUDE.md | Very large (200K + overflow to sub-agents) |
| Cursor | Embedding retrieval + Apply model | Large (retrieves relevant chunks) |
| Aider | Repo map + focused file loading | Medium (repo map is compact) |
| Codex | Prefix caching + fresh per task | Medium (no cross-task memory) |
| Devin | Full VM state | Very large (persistent VM) |
| Windsurf | Persistent flows | Large (maintained across sessions) |
| OpenHands | Event stream | Large (but grows without bound) |

### File Editing Approaches

| System | Method | Reliability |
|--------|--------|-------------|
| Claude Code | Exact string replacement | High — requires exact match, prevents wrong-location edits |
| Cursor | Worker diffs + Apply model | High — specialized merge model |
| Aider | Search/replace with fuzzy match | Medium — fuzzy matching can misfire |
| Codex | Unified diffs | Medium — standard but can fail on large diffs |
| SWE-Agent | ACI edit command | High — constrained editing |
| Devin | Direct file editing in VM | Medium — no structural constraints |

### Testing and Verification

| System | Approach | Review Mechanism |
|--------|----------|-----------------|
| Claude Code | Run tests via Bash | None built-in (user reviews) |
| Cursor | Run tests, feed back to planner | Planner re-plans on failure |
| Aider | Run tests, auto-fix loop | Architect re-analyzes failures |
| Codex | Run tests in sandbox | None built-in |
| Devin | Run tests, Critic reviews | Critic agent reviews changes |
| Harness | Run tests via agent | Review loop (LGTM/FIXED/WAITING) |

### Review Mechanisms

| System | Review Type | Who Reviews |
|--------|------------|-------------|
| Claude Code | Human review (user) | User in conversation |
| Cursor | Background Agent can self-review | Planner agent |
| Devin | Critic agent | Separate Critic agent |
| Harness | Review loop | Separate review prompt (same or different model) |
| Open SWE | Reviewer agent | Dedicated Reviewer agent |
| Copilot Workspace | Human review of plan | User reviews plan before execution |

---

## Part 6: Master Comparison Matrix

### SWE-bench Verified Scores (approximate, early 2026)

| System | Score | Architecture | Cost/Issue |
|--------|-------|-------------|-----------|
| Claude Code (Opus 4) | ~72% | Single agent | ~$5-15 |
| Codex (codex-1) | ~69% | Single agent | ~$3-8 |
| Cursor Agent | ~65% | Multi-agent (Planner/Worker) | ~$2-6 |
| Augment Code | ~60% | Multi-agent (Coord/Specialists) | Unknown |
| Devin | ~55% | Multi-agent (4 agents) | ~$10-30 |
| Aider (Opus) | ~48% | Single agent (Architect/Editor) | ~$2-5 |
| SWE-Agent | ~40% | Single agent (ACI) | ~$1-3 |
| Agentless | ~32% | No agent (pipeline) | ~$0.34 |
| Moatless | ~28% | FSM | ~$0.01 |

Note: Scores are approximate and change frequently with model and system updates.

---

## Part 7: Key Observations

### 1. Architecture vs Model: Scaffolding Matters

The debate about whether architecture or model quality matters more has been largely settled: **both matter, but scaffolding has diminishing returns**.

Evidence:
- Claude Code (simple loop + Opus 4) outperforms Devin (complex multi-agent + weaker model)
- But Aider with Opus matches or exceeds SWE-Agent with Opus, suggesting architecture helps at the margin
- Agentless (no agent at all) achieves 32% with just a pipeline — architecture can substitute for agency
- Moatless achieves 28% for $0.01 — extreme cost efficiency through structure

**Conclusion**: Get the model right first, then optimize architecture for cost and reliability.

### 2. Simple Beats Complex (Often)

Single-agent systems consistently compete with or outperform multi-agent systems:
- Claude Code (single) > Devin (multi) on SWE-bench
- OpenHands-Versa (single) > Magentic-One (multi) on GAIA
- The coordination overhead of multi-agent must be justified by the task complexity

**Exception**: Cursor's parallel workers provide genuine speedup for large cross-file changes. Parallelism (doing independent work simultaneously) justifies multi-agent. Collaboration (agents discussing with each other) often doesn't.

### 3. MCP Convergence

The ecosystem is converging on MCP as the tool interface standard:
- Claude Code: native MCP support
- Cursor: MCP support
- Copilot: MCP support
- Goose: MCP-native (all tools are MCP)
- Cline: MCP support

This creates a shared tool ecosystem — MCP servers written for one agent work with all agents.

### 4. Sandboxing Is Becoming Table Stakes

Every modern coding agent runs in some form of sandbox:
- Codex: Landlock sandbox
- Devin: Isolated VM
- OpenHands: Docker container
- Claude Code: Landlock + permission system
- Copilot Coding Agent: Cloud VM
- Jules: Cloud VM

Unsandboxed code execution is becoming unacceptable for production deployment.

### 5. The Editing Problem Is Harder Than It Looks

File editing is a surprisingly difficult problem for LLM agents:
- **Exact match** (Claude Code): reliable but requires reading file first
- **Unified diffs**: models often generate malformed diffs
- **Search/replace** (Aider): fuzzy matching helps but can misfire
- **Apply model** (Cursor): dedicated small model for merging, trained specifically for this
- **Whole file rewrite**: simple but wasteful for small changes, and can lose content in large files

No system has fully solved this. Cursor's Apply model approach (a specialized, cheap model just for merging edits) is the most innovative solution.

### 6. Context Is the Real Battleground

The primary differentiator between coding agents is **how they manage context**:
- What code gets loaded into context (discovery)
- How much context fits (window management)
- How context is maintained over time (persistence)
- How context is shared between agents (isolation vs sharing)

Systems that get context right perform well regardless of other architectural choices. Systems that get context wrong fail regardless of model quality.

---

## Part 8: Implications for Harness

| Industry Pattern | Harness Current | Recommendation |
|---|---|---|
| Single-agent dominance | Single agent per task | Validated — don't add multi-agent complexity for single tasks |
| Parallel workers (Cursor) | Sprint planner creates parallel tasks | Aligned — parallelize at task level, not within task |
| Review agent | Review loop | Aligned |
| Context engineering | Agent manages context | Could add: structured context injection per task type |
| MCP tools | Agent uses MCP natively | No action needed — agent-level concern |
| Sandboxing | Agent's own sandbox | No additional sandboxing needed at orchestrator level |
| Apply model | N/A | Not applicable — Harness delegates editing to agent |
| Difficulty-aware routing | No routing | Consider: light difficulty assessment for model selection |
| Event-stream (OpenHands) | events.jsonl | Aligned in concept — formalize for coordination |
