# Microsoft Multi-Agent Systems: Magentic-One, AutoGen, and Beyond

## 1. Magentic-One

Microsoft's flagship multi-agent system, designed for general-purpose task completion.

### Architecture

```
                 ┌──────────────────────────┐
                 │       Orchestrator        │
                 │  (Claude/GPT-4 class)     │
                 │                           │
                 │  Maintains: Ledger        │
                 │  Decides: Next agent      │
                 │  Monitors: Progress       │
                 └─────────┬────────────────┘
          ┌────────┬───────┼───────┬──────────┐
          │        │       │       │          │
     ┌────▼───┐ ┌──▼───┐ ┌▼────┐ ┌▼────────┐│
     │  Web   │ │ File │ │Coder│ │Computer  ││
     │ Surfer │ │Surfer│ │     │ │Terminal  ││
     └────────┘ └──────┘ └─────┘ └──────────┘│
                                              │
     Star topology: all agents report         │
     to Orchestrator, never to each other ────┘
```

### The Four Specialists

| Agent | Capabilities | Tools |
|-------|-------------|-------|
| **WebSurfer** | Navigate websites, fill forms, extract content | Playwright browser |
| **FileSurfer** | Read/navigate local files, parse documents | File system access |
| **Coder** | Write and analyze code | Code editor, no execution |
| **ComputerTerminal** | Execute code, run commands | Shell access |

**Key constraint**: Specialists are **stateless** — they don't remember previous turns. All context comes from the Orchestrator via the current prompt.

### The Ledger Mechanism

The Orchestrator maintains a structured "Ledger" — a running document that tracks the reasoning state:

```markdown
## Ledger

### Facts
- The user wants to find the average salary for data scientists in NYC
- We found a CSV file at /data/salaries.csv
- The CSV has columns: name, role, city, salary

### Facts to Look Up
- What is the column delimiter (comma vs tab)?
- Are there header rows?

### Facts to Derive
- Average salary for rows where city="NYC" and role="Data Scientist"

### Educated Guesses
- The file is likely comma-delimited (standard CSV)
- Salary column is likely numeric (may need to strip $ and commas)

### Current Plan
1. [DONE] Find the relevant data file
2. [CURRENT] Have Coder write analysis script
3. [TODO] Have ComputerTerminal execute script
4. [TODO] Report results to user

### Next Agent Assignment
- Agent: Coder
- Task: Write Python script to calculate average salary for Data Scientists in NYC
- Context: File is at /data/salaries.csv, columns are name, role, city, salary
```

**Why the Ledger matters**: It externalizes the Orchestrator's reasoning into a structured format that:
- Prevents "losing track" of progress in long conversations
- Separates facts from assumptions (reducing hallucination)
- Provides explicit planning with progress tracking
- Gives context to stateless specialists without forwarding full conversation

### Performance

- **GAIA benchmark**: ~38% (general-purpose tasks)
- **WebArena**: competitive on web navigation tasks
- **Strengths**: multi-step tasks requiring different modalities (web + code + files)
- **Weaknesses**: single-step tasks (orchestration overhead hurts), tasks requiring deep domain expertise

### Limitations

- **Star topology bottleneck**: all communication goes through Orchestrator — it becomes the bottleneck for fast parallel execution
- **Stateless specialists**: can't build on their own previous work, must receive all context each turn
- **No learning**: no memory across sessions, each task starts from scratch
- **Orchestrator token cost**: maintaining and updating the Ledger consumes significant tokens

---

## 2. AutoGen v0.4

Major rewrite of AutoGen from a conversation-based framework to an actor-model architecture.

### Architecture Evolution

```
AutoGen v0.2 (2023):               AutoGen v0.4 (2025):
─────────────────────               ─────────────────────
GroupChat                           Actor Model
├── Agent A ─┐                      ├── Agent A (actor)
├── Agent B  ├── shared messages    ├── Agent B (actor)
├── Agent C ─┘                      ├── Agent C (actor)
└── Manager (selects speaker)       └── Runtime (message routing)

Conversation-based                  Message-based
All agents see all messages         Typed messages, selective routing
```

### Actor Model

Every agent is an independent actor:
- Has its own mailbox (message queue)
- Processes one message at a time
- Communicates only via typed messages
- Can spawn new actors

```python
# AutoGen v0.4 — typed message protocol
@message
class CodeReviewRequest:
    pr_url: str
    review_focus: list[str]

@message
class CodeReviewResult:
    approved: bool
    comments: list[str]

class ReviewerAgent(Agent):
    @handler(CodeReviewRequest)
    async def handle_review(self, msg: CodeReviewRequest) -> CodeReviewResult:
        # ... review logic ...
        return CodeReviewResult(approved=True, comments=[])
```

### Team Patterns

AutoGen v0.4 provides four pre-built team patterns:

| Pattern | How It Works | Use Case |
|---------|-------------|----------|
| **RoundRobinGroupChat** | Agents take turns in fixed order | Sequential pipeline |
| **SelectorGroupChat** | LLM selects next speaker | Dynamic routing |
| **Swarm** | Agents hand off via function returns (like OpenAI Swarm) | Conversational routing |
| **MagenticOneGroupChat** | Ledger-based orchestration (Magentic-One) | Complex multi-step tasks |

### Key Improvements Over v0.2

1. **Type safety**: messages are typed, not just strings
2. **Isolation**: agents don't share conversation history by default
3. **Scalability**: actor model supports distributed execution
4. **Extensibility**: custom team patterns via composition
5. **Observability**: built-in tracing and message logging

### Criticism

- **Complexity**: the actor model is powerful but significantly harder to reason about than simple function calls
- **Migration burden**: v0.2 to v0.4 is a complete rewrite, no migration path
- **Overhead**: for simple 2-agent scenarios, the actor infrastructure is overkill
- **Documentation**: still catching up to the new architecture

---

## 3. Semantic Kernel

Microsoft's AI application framework, with agent capabilities bolted on.

### Core Identity

Semantic Kernel is primarily an **application SDK** — it helps developers integrate AI into applications. Agent orchestration was added later as `SemanticKernel.Agents`.

### Agent Framework

```
┌────────────────────────────────────────┐
│           AgentGroupChat               │
│  ┌──────────┐  ┌──────────┐           │
│  │ChatComple│  │OpenAIAssi│           │
│  │tionAgent │  │stantAgent│  ...      │
│  └──────────┘  └──────────┘           │
│                                        │
│  Selection Strategy:                   │
│  - Sequential                          │
│  - KernelFunction (custom)             │
│                                        │
│  Termination Strategy:                 │
│  - MaxTurns                            │
│  - KernelFunction (custom)             │
└────────────────────────────────────────┘
```

### Process Framework

Semantic Kernel's Process Framework adds DAG-based orchestration:

```python
process = KernelProcess.create("CodeReviewProcess")

# Define steps
analyze = process.add_step(AnalyzeCodeStep)
review = process.add_step(ReviewCodeStep)
approve = process.add_step(ApproveStep)

# Define edges (DAG)
process.on_event("start").send_to(analyze)
analyze.on_event("analyzed").send_to(review)
review.on_event("approved").send_to(approve)
review.on_event("rejected").send_to(analyze)  # cycle for revision
```

### Position in Microsoft Ecosystem

- **Semantic Kernel**: application-level AI SDK (enterprise customers)
- **AutoGen**: research-grade multi-agent framework (researchers, advanced developers)
- **Magentic-One**: specific multi-agent system (benchmarking, demos)

They overlap significantly but target different audiences.

---

## 4. TaskWeaver

### Concept

TaskWeaver converts natural language tasks into **code** as the universal action language. Instead of using pre-defined tools, it generates Python code to accomplish any task.

### Architecture

```
User Request
     │
     ▼
┌──────────┐     ┌──────────────┐     ┌──────────────┐
│ Planner  │ ──► │ Code         │ ──► │ Code         │
│          │     │ Generator    │     │ Executor     │
│ NL → Plan│     │ Plan → Python│     │ Python → Run │
└──────────┘     └──────────────┘     └──────────────┘
                                           │
                                           ▼
                                      ┌──────────┐
                                      │ Results  │
                                      └──────────┘
```

### Flow

1. **Planner**: decomposes user request into step-by-step plan (natural language)
2. **Code Generator**: translates each plan step into executable Python code
3. **Code Executor**: runs the generated code in a sandboxed environment
4. **Results**: returns execution results to user

### Key Insight

**Code as universal action language** — instead of:
- Defining 50 tools for different data operations
- Managing tool schemas and parameter validation
- Handling tool composition manually

...TaskWeaver generates code that composes any operation naturally:

```python
# Instead of tool calls:
#   search_files(pattern="*.csv") → read_csv(file) → filter(col="city", val="NYC") → mean(col="salary")
# TaskWeaver generates:
import pandas as pd
df = pd.read_csv("/data/salaries.csv")
result = df[df["city"] == "NYC"]["salary"].mean()
print(f"Average salary: ${result:,.2f}")
```

### Tradeoffs

**Pros**: Maximum flexibility, no tool definition overhead, natural composition
**Cons**: Security risk (arbitrary code execution), harder to audit, no type safety on actions

---

## 5. GitHub Copilot Workspace

### Architecture

A four-phase single pipeline (not multi-agent):

```
Issue/Task Description
     │
     ▼
┌──────────┐     ┌──────────┐     ┌──────────┐     ┌──────────┐
│   Spec   │ ──► │   Plan   │ ──► │Implement │ ──► │ Validate │
│          │     │          │     │          │     │          │
│ Clarify  │     │ Which    │     │ Generate │     │ Build +  │
│ what to  │     │ files to │     │ code     │     │ test the │
│ change   │     │ change   │     │ changes  │     │ changes  │
└──────────┘     └──────────┘     └──────────┘     └──────────┘
     ↕                ↕                ↕                ↕
  [Human can edit/override at any stage]
```

### Key Characteristics

- **Not multi-agent**: single pipeline with four phases
- **Human-in-the-loop**: user can edit the spec, plan, or implementation at any stage
- **Deterministic phases**: each phase has clear input/output contracts
- **GitHub-integrated**: operates directly on repos, branches, PRs

### Phases

1. **Spec**: from issue description + repo context, generate a clear specification of what needs to change
2. **Plan**: identify specific files and changes needed
3. **Implement**: generate the actual code changes
4. **Validate**: run CI, tests, and analysis

### What It's Not

- Not a conversational agent — no iterative refinement loop
- Not multi-agent — no specialist delegation
- Not autonomous — human review required at each stage

---

## 6. Recurring Pattern Across Microsoft Systems

Despite different architectures, all Microsoft systems share three structural elements:

### Pattern 1: Centralized Orchestrator

Every system has a single coordination point:
- Magentic-One: Orchestrator agent
- AutoGen: GroupChatManager or custom Team
- Semantic Kernel: Process framework
- TaskWeaver: Planner
- Copilot Workspace: Pipeline controller

**No decentralized/peer-to-peer coordination** in any production Microsoft system.

### Pattern 2: Structured Intermediate State

Every system maintains explicit structured state between steps:
- Magentic-One: **Ledger** (facts, plan, assignments)
- AutoGen: **Typed messages** between actors
- Semantic Kernel: **Process state** in DAG
- TaskWeaver: **Plan** (natural language step list)
- Copilot Workspace: **Spec** and **Plan** documents

This intermediate state serves dual purpose:
1. Context management (what has been done, what remains)
2. Human observability (users can inspect and modify the state)

### Pattern 3: Planning/Execution Separation

Every system separates "deciding what to do" from "doing it":
- Magentic-One: Orchestrator plans → Specialists execute
- AutoGen: Team pattern selects → Agent acts
- Semantic Kernel: Process graph defines → Steps execute
- TaskWeaver: Planner plans → Code Generator + Executor execute
- Copilot Workspace: Spec + Plan → Implement + Validate

**Why this matters**: Separating planning from execution allows:
- Independent evaluation of plan quality
- Human override of plans before execution
- Re-planning without re-executing completed steps
- Different models for planning (smart) vs execution (fast)

---

## 7. Implications for Harness

| Microsoft Pattern | Harness Current | Alignment |
|---|---|---|
| Centralized orchestrator | Server + task_runner | Aligned |
| Structured intermediate state | events.jsonl, task metadata | Partial — could be more structured |
| Planning/execution separation | sprint_planner + agent execution | Aligned |
| Ledger mechanism | Not implemented | High value — formalize task state tracking |
| Typed messages | Prompts (untyped) | Gap — but acceptable for single-agent |
| Code as action | Agent uses bash/tools | Similar in practice |

### Highest-Value Adoption

The **Ledger mechanism** from Magentic-One is the most directly applicable pattern:
- Formalize the "what we know / what we need to find out / current plan" structure
- Include it in every agent prompt as structured context
- Update it after each agent turn
- Use it for impasse detection ("no facts added in 3 turns → stuck")

This is compatible with Harness's current architecture and doesn't require multi-agent infrastructure.
