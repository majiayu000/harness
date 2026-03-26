# Anthropic Agent Architecture: Research, Principles, and Production Systems

## 1. Multi-Agent Research System

Anthropic's internal multi-agent research system represents their most ambitious exploration of agent coordination.

### Architecture

```
┌──────────────────────────────────────────┐
│              Opus 4 Lead Agent            │
│  - Decomposes research question          │
│  - Assigns sub-questions to workers      │
│  - Synthesizes final answer              │
└────────┬──────────┬──────────┬───────────┘
         │          │          │
    ┌────▼───┐ ┌────▼───┐ ┌────▼───┐
    │Sonnet 4│ │Sonnet 4│ │Sonnet 4│  ... (N subagents)
    │Worker  │ │Worker  │ │Worker  │
    │        │ │        │ │        │
    └────────┘ └────────┘ └────────┘
```

- **Lead**: Claude Opus 4 — handles decomposition, delegation, synthesis
- **Workers**: Claude Sonnet 4 — execute individual research subtasks
- **Communication**: Structured tool calls from lead to workers

### Results

- **90.2% improvement** on complex research tasks compared to single-agent baseline
- **15x token cost** compared to single-agent — the multi-agent tax is real
- **Key tradeoff**: accuracy gains justify cost only for high-value, complex queries

### Early Failures: The 50+ Subagent Problem

Initial experiments revealed a critical failure mode:

> "In early iterations, the lead agent would spawn 50+ subagents for even simple queries, burning through context and budget with no quality improvement."

The fix: explicit scaling rules that constrain when multi-agent is appropriate:
- Simple factual queries: single agent, no subagents
- Multi-step research: 3-7 subagents, each with focused scope
- Exhaustive analysis: up to 15 subagents, hierarchical coordination

This directly led to the "scaling rules" principle (see Section 2).

---

## 2. Eight Prompt Engineering Principles

Anthropic distilled their agent development experience into eight principles, published alongside the multi-agent research system.

### Principle 1: Mental Model Building

Give agents explicit mental models of how systems work, not just instructions for what to do.

```
BAD:  "Search the codebase for relevant files"
GOOD: "The codebase is organized as a Rust workspace with 10 crates under crates/.
       Core business logic is in harness-core, HTTP handling in harness-server.
       Start with the crate most relevant to the task, then explore dependencies."
```

### Principle 2: Delegation Frameworks

Provide structured frameworks for how lead agents should delegate to subagents:
- What information to include in the delegation prompt
- What information to withhold (context isolation)
- How to specify success criteria for the subtask
- How to handle partial or failed results

### Principle 3: Scaling Rules

Explicit rules for when to scale up vs. stay single-agent:
- **Complexity threshold**: only spawn subagents when task requires 3+ independent information sources
- **Diminishing returns**: cap subagent count based on task type
- **Cost awareness**: include estimated token cost in scaling decisions

### Principle 4: Tool Design

Tools should be designed for agent consumption, not human consumption:
- Descriptive names that convey purpose (`search_codebase` not `search`)
- Rich descriptions with usage examples
- Structured output that's easy for models to parse
- Error messages that guide the agent toward correct usage

### Principle 5: Self-Improvement

Agents should be able to refine their approach based on intermediate results:
- After initial search yields poor results, reformulate query
- After failed tool call, try alternative tool or parameters
- After partial answer, identify and fill gaps

### Principle 6: Breadth-First Exploration

For research tasks, explore breadth-first before going deep:
- First pass: scan all potentially relevant sources
- Second pass: deep-dive into the most promising sources
- Prevents premature commitment to a single information thread

### Principle 7: Extended and Interleaved Thinking

Use Claude's extended thinking capability strategically:
- Enable for complex reasoning steps (planning, synthesis)
- Interleave thinking with tool use (think → act → think → act)
- Don't force thinking on simple retrieval or tool execution steps

### Principle 8: Parallelization

When subagents are independent, run them in parallel:
- Identify which subtasks have no data dependencies
- Launch independent workers simultaneously
- Merge results after all workers complete
- Handle partial failures gracefully (don't fail the whole task if one worker fails)

---

## 3. "Building Effective Agents" Guide

Anthropic's published guide (December 2024, updated 2025) presents a pragmatic framework for agent development. The core message: **"Start with the simplest solution that could work."**

### Five Workflow Patterns

#### Pattern 1: Prompt Chaining

```
Input → LLM Step 1 → Gate → LLM Step 2 → Gate → Output
```

- Sequential steps with validation gates between them
- Each step has a focused task
- Gates check quality before proceeding
- **Use when**: task naturally decomposes into sequential stages

#### Pattern 2: Routing

```
Input → Classifier → Route A → Output
                   → Route B → Output
                   → Route C → Output
```

- Classify input, then dispatch to specialized handler
- Each route can have different models, prompts, tools
- **Use when**: different input types need fundamentally different handling

#### Pattern 3: Parallelization

```
Input → Split → Worker A ──┐
              → Worker B ──┼→ Merge → Output
              → Worker C ──┘
```

- Divide work into independent chunks, process simultaneously, merge
- Two variants: **sectioning** (different aspects) and **voting** (same task, majority wins)
- **Use when**: subtasks are independent and benefit from parallel execution

#### Pattern 4: Orchestrator-Workers

```
Input → Orchestrator → Worker A → Orchestrator → Worker B → ... → Output
```

- Central orchestrator dynamically assigns tasks to workers
- Orchestrator sees all results and decides next steps
- Workers are specialized and focused
- **Use when**: task requires dynamic planning that can't be predetermined

#### Pattern 5: Evaluator-Optimizer

```
Input → Generator → Evaluator → [Good enough?] → Output
                               → [No] → Generator (with feedback)
```

- Generate candidate output, evaluate quality, iterate if needed
- Evaluator can be a different model, a set of rules, or human judgment
- **Use when**: output quality is critical and can be objectively evaluated

### The Hierarchy

```
Simple ──────────────────────────────────────────── Complex

Single LLM call → Prompt Chain → Routing → Parallel → Orchestrator → Evaluator-Optimizer
```

**Always start at the left and move right only when necessary.** Most tasks don't need multi-agent orchestration.

---

## 4. Claude Code

Claude Code is Anthropic's production coding agent, representing their most mature agent implementation.

### Architecture: The "nO" Loop

```
while True:
    user_input = get_input()
    while True:
        response = claude.generate(messages, tools)
        if response.has_tool_calls():
            results = execute_tools(response.tool_calls)
            messages.extend(results)
        else:
            display(response.text)
            break
```

- **Single-threaded**: one agent, one conversation, sequential tool execution
- **No explicit planning step**: the model decides what to do next based on conversation context
- **Tools**: file read/write/edit, bash execution, glob, grep, web search
- **No retrieval system**: uses glob + grep for codebase navigation (not embeddings, not vector DB)

### Depth-Limited Subagents

Claude Code supports spawning subagents for parallel work:

```
Main Agent
├── Subagent 1 (search task)
├── Subagent 2 (analysis task)
└── Subagent 3 (implementation task)
```

- **Depth limit**: subagents cannot spawn their own subagents (prevents recursive explosion)
- **Context isolation**: each subagent gets only the relevant portion of context
- **Tool restriction**: subagents have reduced tool access (typically read-only)

### Agent Teams (Experimental)

- 3-5 "teammates" working on related tasks in a shared repository
- Each teammate has its own conversation and tool access
- Coordination through git (shared branch, merge conflicts as signals)
- **Experimental status**: available but not recommended for production

### Key Design Decisions

1. **No embeddings**: glob + grep outperforms embedding search for code navigation because code structure is hierarchical, not semantic
2. **No persistent memory**: each session starts fresh (explicit choice for simplicity)
3. **No planning overhead**: direct tool execution instead of plan-then-execute
4. **Sandboxing**: tool execution in sandboxed environment (Landlock on Linux)

---

## 5. Claude Agent SDK

A Python framework for building custom agents with Claude, released alongside the Agents API.

### Core Components

```python
import claude_agent_sdk as sdk

agent = sdk.Agent(
    model="claude-sonnet-4-20250514",
    tools=[search_tool, write_tool],
    system_prompt="You are a code reviewer...",
    max_turns=20,
)

result = agent.run("Review the PR at https://...")
```

### Features

- **Built-in agent loop**: handles the think → act → observe cycle
- **Tool management**: register tools with schemas, automatic parameter validation
- **Context management**: configurable context window strategies
- **Permissions**: fine-grained tool permission system
- **Hooks**: pre/post execution hooks for logging, validation, transformation
- **Streaming**: token-level and event-level streaming

### Hooks System

```python
@agent.hook("pre_tool_call")
def validate_write(tool_name, params):
    if tool_name == "file_write" and "/etc/" in params["path"]:
        raise PermissionError("Cannot write to /etc/")

@agent.hook("post_turn")
def log_turn(turn_number, messages):
    logger.info(f"Turn {turn_number}: {len(messages)} messages")
```

---

## 6. Context Engineering

Anthropic's research on managing context effectively in long-running agent sessions.

### The Context Rot Problem

As conversations grow, agent performance degrades:
- **Relevant information** gets buried in conversation history
- **Contradictory information** from earlier turns confuses the model
- **Tool output noise** fills context with irrelevant details
- **Lost-in-the-middle** effect: model pays less attention to mid-conversation content

### Four Management Strategies

#### Strategy 1: Compaction (Summarization)

Periodically summarize conversation history to reduce token count:
- Preserve key decisions and constraints
- Remove intermediate exploration steps
- Keep file paths and code snippets that are still relevant
- Risk: important context can be lost in summarization

#### Strategy 2: Structured Note-Taking

Maintain a structured scratchpad alongside conversation:
- Agent writes notes to a structured format after key discoveries
- Notes are always included in context (pinned)
- Conversation history can be aggressively truncated because notes capture essentials
- Claude Code's CLAUDE.md and memory files follow this pattern

#### Strategy 3: Sub-Agents for Context Isolation

Delegate subtasks to sub-agents with fresh, focused context:
- Main agent retains high-level state
- Sub-agent gets only the relevant context for its specific task
- Sub-agent returns a focused result, not its entire conversation
- Prevents context pollution from tangential exploration

#### Strategy 4: Tool Search Tool

Instead of loading all tool definitions into context, use a meta-tool that searches for relevant tools:
- **85% context reduction** from tool definitions
- Agent calls `search_tools("database query")` → gets back relevant tool schemas
- Only relevant tools are loaded into context for that turn
- Particularly valuable when agent has access to 50+ tools

### Context Budget Framework

```
Total Context Window: 200K tokens
├── System prompt + rules: 5K (fixed)
├── Tool definitions: 2K (dynamic via Tool Search)
├── Conversation history: 100K (compacted periodically)
├── Active file contents: 50K (loaded on demand)
├── Structured notes: 10K (persistent)
└── Reserve for generation: 33K (always maintained)
```

---

## 7. Advanced Tool Use

### Tool Search Tool

```
Agent: "I need to query the database"
  → search_tools("database query")
  → Returns: [sql_query, db_schema, db_migrate]
  → Agent picks sql_query, uses it

Agent: "I need to send a notification"
  → search_tools("notification send")
  → Returns: [slack_send, email_send, push_notify]
  → Agent picks appropriate tool
```

- **85% reduction** in tool-definition context usage
- Scales to hundreds of tools without context bloat
- Agent learns to search before acting

### Programmatic Tool Calling

Instead of the model choosing tools via natural language, provide a structured decision framework:

```
Given the current state:
- Files modified: [list]
- Tests status: [pass/fail]
- Remaining tasks: [list]

Select next action from:
1. edit_file(path, changes) — if implementation needed
2. run_tests() — if verification needed
3. search_code(query) — if more context needed
4. complete() — if all tasks done
```

- **37% token reduction** compared to free-form tool selection
- More predictable tool usage patterns
- Easier to audit and debug

### Tool Use Examples

Including examples in tool descriptions dramatically improves tool use accuracy:

```json
{
  "name": "edit_file",
  "description": "Edit a file by replacing old content with new content",
  "examples": [
    {
      "input": {"path": "src/main.rs", "old": "fn main() {}", "new": "fn main() {\n    println!(\"hello\");\n}"},
      "explanation": "Add a print statement to main function"
    }
  ]
}
```

---

## 8. Anthropic's Position on Multi-Agent Systems

Anthropic's consistent position across publications, talks, and product decisions:

> **Multi-agent is a scaling technique for token capacity and parallelism, not an inherently superior architecture.**

### When Multi-Agent Helps

1. **Token capacity**: single context window can't hold all necessary information
2. **Parallelism**: independent subtasks that can execute simultaneously
3. **Specialization**: different subtasks require fundamentally different tool sets or system prompts
4. **Cost optimization**: using cheaper models for simple subtasks, expensive models for complex ones

### When Multi-Agent Hurts

1. **Simple tasks**: orchestration overhead exceeds task complexity
2. **Tightly coupled steps**: sequential dependencies negate parallelism benefits
3. **Small context**: task fits comfortably in single context window
4. **Coordination costs**: inter-agent communication introduces latency and errors

### The Pragmatic Hierarchy

```
1. Single LLM call (most tasks)
2. Single agent with tools (complex tasks)
3. Single agent with sub-agents (parallel/specialized subtasks)
4. Multi-agent system (only when 1-3 are insufficient)
```

This hierarchy aligns with the "Building Effective Agents" guide: always start simple, add complexity only when justified by measurable improvement.

---

## 9. Implications for Harness

| Anthropic Concept | Harness Current | Recommendation |
|---|---|---|
| nO loop | task_runner is similar | Aligned |
| Depth-limited subagents | No subagent support | Consider for parallel subtask execution |
| Context compaction | No context management | Agent handles this internally |
| Tool Search Tool | N/A (agent manages tools) | Not needed — agent-level concern |
| Scaling rules | No explicit rules | Add guidelines for when to use multi-task vs single-task |
| input_filter pattern | No context shaping | Implement for task chaining |
| Evaluator-optimizer | Review loop | Aligned — review loop is this pattern |

### Key Takeaway

Anthropic's research validates a conservative approach: invest in making single agents better (better prompts, better tools, better context management) before adding multi-agent complexity. Harness's current single-agent-per-task model is appropriate for most workflows. Multi-agent coordination should be reserved for genuinely parallelizable work like reviewing multiple PRs simultaneously.
