# OpenAI Swarm & Agents SDK

## 1. Swarm (October 2024)

OpenAI released Swarm as an **educational framework** for exploring multi-agent orchestration patterns. It was explicitly labeled "not production-ready" and intended as a teaching tool.

### Core Design

- ~300 lines of Python code total
- **Stateless handoffs**: no persistent memory between turns; each call to `run()` is self-contained
- Agent = `{name, instructions, functions}` — a thin wrapper around a system prompt + tool list
- Handoff = a function that returns another Agent object; the runner switches the active agent
- Context variables passed as a mutable dict — the only shared state mechanism

### Execution Model

```python
# Simplified Swarm loop
while True:
    response = client.chat.completions.create(
        model=agent.model,
        messages=messages,
        tools=agent.functions,
    )
    if response.tool_calls:
        for call in response.tool_calls:
            result = execute(call)
            if isinstance(result, Agent):
                agent = result  # handoff
            else:
                messages.append(tool_result(call, result))
    else:
        break  # no more tool calls, done
```

### What It Got Right

- Proved that multi-agent handoffs can be reduced to function returns
- Demonstrated that "agents" are just (instructions, tools) tuples — no special infrastructure needed
- Made the control flow obvious: the LLM decides when to hand off by calling a function

### What It Lacked

- No persistence across calls
- No tracing or observability
- No guardrails or safety mechanisms
- No input/output filtering during handoffs
- No production error handling

---

## 2. Agents SDK (March 2025)

The production successor to Swarm. Ships as `openai-agents` Python package. Keeps the same philosophy (LLM controls flow via tool calls) but adds the missing production pieces.

### Four Primitives

| Primitive | Purpose | Key Feature |
|-----------|---------|-------------|
| **Agent** | Instructions + model + tools | `output_type` for structured output validation |
| **Handoff** | Agent-to-agent transition | `input_filter` to transform/reduce context |
| **Guardrails** | Input/output validation | Run in parallel with agent, can halt execution |
| **Tracing** | Observability | OpenTelemetry-compatible, auto-traces all steps |

### Runner Architecture

The Runner is a **while loop**, not a DAG:

```python
class Runner:
    async def run(agent, input, max_turns=10):
        current_agent = agent
        turn = 0
        while turn < max_turns:
            response = await current_agent.generate(messages)
            if response.handoff:
                current_agent = response.handoff.target
                messages = response.handoff.input_filter(messages)  # KEY
            elif response.tool_calls:
                results = await execute_tools(response.tool_calls)
                messages.extend(results)
            elif response.final_output:
                return response.final_output
            turn += 1
```

Key characteristics:
- **Single active agent** at any time — no parallel agent execution
- **max_turns=10** default prevents runaway loops
- LLM decides flow entirely through tool calls and handoff function calls
- No developer-defined graph edges, no state machine transitions

### Handoff input_filter: The Key Innovation

The most important addition over Swarm. When Agent A hands off to Agent B, `input_filter` controls what context Agent B receives:

```python
def summarize_for_specialist(messages):
    """Strip irrelevant conversation, keep only the extracted task."""
    return [msg for msg in messages if msg.role == "tool" or "task:" in msg.content.lower()]

handoff_to_coder = Handoff(
    target=coder_agent,
    input_filter=summarize_for_specialist,
)
```

This solves **context pollution** — the #1 practical problem in multi-agent systems where Agent B inherits all of Agent A's irrelevant conversation history, degrading its performance.

### Guardrails

Run in parallel with the agent (not sequentially):

```python
class ContentGuardrail(InputGuardrail):
    async def check(self, input, context):
        result = await classify(input)
        if result.is_harmful:
            return GuardrailResult(tripwire=True, message="Blocked")
        return GuardrailResult(tripwire=False)
```

- Input guardrails: checked before agent processes
- Output guardrails: checked after agent generates
- Can use a separate (cheaper/faster) model for classification

### Tracing

Built-in OpenTelemetry-compatible tracing:
- Every agent turn, tool call, handoff, and guardrail check is automatically traced
- Integrates with OpenAI's dashboard and any OTLP-compatible backend
- Custom spans via `@trace` decorator

---

## 3. Comparison Matrix: Agents SDK vs LangGraph vs CrewAI

| Dimension | Agents SDK | LangGraph | CrewAI |
|-----------|-----------|-----------|--------|
| **Who controls flow?** | LLM (tool calls/handoffs) | Developer (graph edges) | Framework (task lists) |
| **Flow definition** | Implicit via tools | Explicit StateGraph | Declarative task YAML |
| **Parallel agents** | No (single active) | Yes (Send/fan-out) | Yes (parallel tasks) |
| **Persistence** | None built-in | Checkpointing (SQLite/Postgres) | Optional memory (short/long-term) |
| **Human-in-the-loop** | None built-in | First-class (interrupt_before/after) | Optional (human_input=True on task) |
| **Streaming** | Token-level only | 6 streaming modes | Basic token streaming |
| **Context management** | input_filter on handoffs | State channels + reducers | Shared memory object |
| **Guardrails** | Built-in primitive | Custom node logic | No built-in |
| **Tracing** | Built-in OTLP | LangSmith integration | No built-in |
| **Complexity** | Low (~300 lines core) | High (graph DSL) | Medium (config-driven) |
| **Best for** | Simple chatbot routing | Complex stateful workflows | Role-based team tasks |
| **Worst for** | Long-running workflows | Quick prototypes | Low-level control |

### Agent Modeling

| Aspect | Agents SDK | LangGraph | CrewAI |
|--------|-----------|-----------|--------|
| Agent definition | (instructions, model, tools) | Node function | (role, goal, backstory, tools) |
| Agent communication | Handoff (context transfer) | Shared state graph | Delegation / shared memory |
| Agent discovery | Static (code-defined) | Static (graph-defined) | Static (crew-defined) |
| Specialization | Via instructions + tool subset | Via node logic | Via role + backstory prompt |

---

## 4. The Fundamental Question

The most important architectural decision in agent orchestration is: **who controls the flow?**

### Option A: LLM Controls Flow (Agents SDK)

```
Human → Agent A → [LLM decides] → Tool Call / Handoff / Final Output
```

- **Advantage**: Maximum flexibility, agents can adapt to unexpected situations
- **Disadvantage**: Non-deterministic, hard to guarantee specific workflows execute
- **When to use**: Conversational routing, open-ended tasks, when you trust the model

### Option B: Developer Controls Flow (LangGraph)

```
Human → Node A → [Edge condition] → Node B → [Edge condition] → Node C
```

- **Advantage**: Deterministic paths, guaranteed execution order, debuggable
- **Disadvantage**: Rigid, requires anticipating all paths at design time
- **When to use**: Business workflows, compliance-critical paths, complex state machines

### Option C: Framework Controls Flow (CrewAI)

```
Human → Task 1 (Agent A) → Task 2 (Agent B) → Task 3 (Agent C)
```

- **Advantage**: Easiest to set up, intuitive "team" metaphor
- **Disadvantage**: Limited flexibility, framework makes assumptions
- **When to use**: Predictable multi-step pipelines, content generation chains

### The Hybrid Reality

In practice, production systems combine approaches:
- **Outer loop**: Developer-controlled (ensure critical steps happen)
- **Inner loop**: LLM-controlled (let agents figure out implementation details)
- **Task level**: Framework-controlled (route to the right agent/queue)

Harness follows this hybrid: the task runner (developer-controlled outer loop) manages lifecycle, while the agent (LLM-controlled inner loop) decides how to execute each step.

---

## 5. Implications for Harness

| Agents SDK Feature | Harness Equivalent | Gap |
|---|---|---|
| Handoff input_filter | None — full context passed | Should implement context shaping per task type |
| Guardrails | Review loop (post-hoc) | No pre-execution guardrails |
| Tracing | events.jsonl | Similar pattern, less structured |
| max_turns | No limit | Should add configurable turn limits |
| Single active agent | Single agent per task | Aligned — but no handoff between agents |

### Key Takeaway

The Agents SDK validates Harness's core assumption: a simple while loop with tool execution is sufficient for most agent workflows. The main gap is **context management during transitions** — Harness should adopt the input_filter pattern when chaining tasks or handing off between agents.
