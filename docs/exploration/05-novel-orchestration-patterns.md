# Novel Orchestration Patterns for Agent Systems

Eight patterns drawn from distributed systems, robotics, cognitive science, and AI research — evaluated for applicability to LLM agent orchestration.

---

## Pattern 1: Blackboard Architecture

### Concept

A shared workspace (the "blackboard") where multiple independent agents read from and write to. Agents are triggered by changes to the blackboard state rather than by explicit messages.

```
┌─────────────────────────────────┐
│          BLACKBOARD              │
│  ┌─────────┐ ┌────────────────┐ │
│  │ Problem  │ │ Partial        │ │
│  │ State    │ │ Solutions      │ │
│  └─────────┘ └────────────────┘ │
│  ┌──────────────┐ ┌───────────┐ │
│  │ Constraints  │ │ Metadata  │ │
│  └──────────────┘ └───────────┘ │
└──────┬──────┬──────┬──────┬─────┘
       │      │      │      │
    ┌──▼──┐┌──▼──┐┌──▼──┐┌──▼──┐
    │Agent││Agent││Agent││Agent│
    │  A  ││  B  ││  C  ││  D  │
    └─────┘└─────┘└─────┘└─────┘
```

**Key properties**:
- Agents don't communicate directly — only through the blackboard
- Any agent can read any part of the blackboard
- Agents write their contributions; a controller decides which agent acts next
- Naturally supports opportunistic problem-solving

### LLM Implementations

- **LangGraph shared state**: StateGraph channels function as a blackboard. Each node reads from and writes to shared state, with reducers handling concurrent writes.
- **AutoGen GroupChat**: agents share a message board (conversation history). The GroupChatManager selects the next speaker based on board state.
- **MetaGPT SharedEnvironment**: all agents share an environment object containing the current project state (requirements, designs, code, tests).

### Feasibility for Harness: HIGH

Harness already has elements of blackboard architecture:
- `events.jsonl` serves as a shared log that any component can read
- Git repository is the ultimate shared state
- Task metadata in SQLite captures partial progress

**Implementation path**: Formalize the blackboard as a structured shared state (project state: files changed, tests status, review status, blockers) that tasks can read from and write to. This enables tasks to react to each other's progress without direct communication.

---

## Pattern 2: Stigmergy (Indirect Coordination)

### Concept

Agents coordinate indirectly by modifying the shared environment, rather than communicating directly. Inspired by how ants leave pheromone trails — no ant talks to another, but collective behavior emerges from environmental signals.

```
Agent A commits code → Git history changes
Agent B reads git log → Sees A's changes → Adapts its approach
Agent C reads CLAUDE.md → Sees updated conventions → Follows them
```

**Key properties**:
- No direct inter-agent messaging
- Environment serves as both communication medium and memory
- Scales naturally — adding agents doesn't increase communication complexity
- Robust to agent failure — signals persist in the environment

### LLM Implementations

- **Git as stigmergy**: Claude Code Agent Teams use git commits as coordination signals. Each agent works on its branch, and merge conflicts act as natural conflict detection.
- **CLAUDE.md as pheromone**: shared project conventions that all agents read and (optionally) update.
- **File markers**: agents create/modify sentinel files (e.g., `.review-needed`, `TODO.md`) that other agents react to.

### Feasibility for Harness: MEDIUM-HIGH

Harness already uses stigmergy patterns without naming them:
- Git commits are the primary coordination mechanism between agents
- CLAUDE.md provides shared conventions
- PR descriptions carry context between tasks

**Enhancement path**: Make stigmergic signals explicit and structured:
- Standardized commit message tags that agents parse (e.g., `[BLOCKS: task-123]`)
- Shared state files in `.harness/` directory
- Event emissions that other tasks can subscribe to

---

## Pattern 3: Market-Based Task Allocation

### Concept

Tasks are announced to a marketplace of agents. Agents bid based on their capability, availability, and cost. The orchestrator awards tasks to the best bidder.

```
Orchestrator: "Who can review this Rust PR? Budget: $0.50, Deadline: 5 min"
    │
    ├── Agent A (Claude Opus): "I can do it for $0.45, ETA 3 min, confidence 95%"
    ├── Agent B (GPT-4o): "I can do it for $0.30, ETA 4 min, confidence 85%"
    ├── Agent C (Sonnet): "I can do it for $0.15, ETA 2 min, confidence 75%"
    │
Orchestrator: Awards to Agent B (best cost/quality/time balance)
```

**Key properties**:
- Decentralized decision-making about task-agent matching
- Natural load balancing through bidding
- Cost optimization through competition
- Handles heterogeneous agent capabilities

### LLM Implementations

- **RouteLLM**: routes requests to different models based on cost-quality tradeoffs. Not a full market, but price-aware routing.
- **Fetch.ai**: actual agent marketplace with economic incentives. Agents have wallets, exchange tokens for services.
- **DAAO (Paper 6)**: difficulty-aware routing is a simplified version — assess difficulty, route to appropriately-priced agent.
- **Martian Model Router**: real-time model selection based on task characteristics and model pricing.

### Feasibility for Harness: MEDIUM

The full market-based pattern is over-engineered for Harness's current scope, but elements are valuable:
- **Model routing**: select model based on task difficulty (cheap model for simple tasks)
- **Cost budgets**: set per-task cost limits, fail fast if exceeded
- **Capability matching**: route tasks to agents with appropriate tool access

**Implementation path**: Add a lightweight routing layer that considers task type, estimated complexity, and configured model preferences.

---

## Pattern 4: Hierarchical Task Network (HTN)

### Concept

Tasks are decomposed hierarchically from abstract goals to concrete primitive actions. Each level of the hierarchy specifies how to achieve its parent task.

```
Goal: "Fix GitHub issue #42"
├── Task: "Understand the issue"
│   ├── Action: Read issue description
│   ├── Action: Read referenced files
│   └── Action: Reproduce the bug
├── Task: "Implement fix"
│   ├── Action: Modify source file
│   └── Action: Add test
└── Task: "Submit fix"
    ├── Action: Create branch
    ├── Action: Commit changes
    └── Action: Open PR
```

**Key properties**:
- Top-down decomposition from goals to actions
- Each non-leaf task has decomposition methods (multiple ways to achieve it)
- Methods have preconditions — planner selects applicable method
- Naturally handles task dependencies and ordering

### LLM Implementations

- **HuggingGPT**: decomposes complex tasks into API calls to specialized models. Task decomposition → API selection → execution → aggregation.
- **ADaPT (Adaptive Decomposition for AI Tasks)**: dynamically decomposes tasks based on intermediate results. If a subtask fails, re-decomposes at a finer granularity.
- **Harness sprint_planner**: already implements informal HTN — decomposes issues into tasks with dependencies.
- **GitHub Copilot Workspace**: Spec → Plan → Implement → Validate is a fixed HTN.

### Feasibility for Harness: VERY HIGH

HTN formalizes what Harness's sprint_planner already does informally:

```
Current: sprint_planner → [task1, task2, task3] (flat list, implicit ordering)
HTN:     sprint_planner → tree of tasks with explicit preconditions and methods
```

**Implementation path**:
1. Add `preconditions` to task definition (e.g., "task-2 requires task-1 completed")
2. Add `decomposition_methods` — multiple ways to achieve a task
3. Planner selects method based on preconditions and current state
4. Support re-decomposition when a subtask fails (ADaPT pattern)

This is the highest-value pattern for Harness evolution.

---

## Pattern 5: Subsumption Architecture

### Concept

From robotics (Rodney Brooks, 1986). Behaviors are organized in layers, where higher layers can **subsume** (override) lower layers. Lower layers handle basic reactive behavior; higher layers handle complex reasoning.

```
Layer 3: Strategic planning (long-term goals)
    ↓ subsumes
Layer 2: Task execution (implement, test, commit)
    ↓ subsumes
Layer 1: Safety (don't delete production data, don't push to main)
    ↓ subsumes
Layer 0: Reflexes (if error, stop; if stuck, ask for help)
```

**Key properties**:
- Lower layers run continuously and independently
- Higher layers activate when their conditions are met
- Higher layers can override lower layer outputs
- System degrades gracefully — if higher layers fail, lower layers still work

### LLM Implementations

- **ReAct (Reasoning + Acting)**: two layers — reasoning layer plans, action layer executes. Reasoning can override action decisions.
- **Reflexion**: adds a reflection layer on top of ReAct that reviews past actions and provides corrective feedback.
- **Constitutional AI**: safety layer that reviews and overrides agent outputs.
- **Guardrails (OpenAI Agents SDK)**: input/output guardrails run as a subsumption layer.

### Feasibility for Harness: MEDIUM

The full subsumption architecture is complex, but the **safety layer** concept is directly valuable:

```
Agent Output → Safety Check (Layer 1) → Review Check (Layer 2) → Approve
                    │                         │
                    ├── Block dangerous        ├── Request changes
                    │   commands               │
                    └── Flag sensitive          └── LGTM
                        file changes
```

**Implementation path**: Add pre-execution guardrails as a subsumption layer:
- Layer 0: Refuse to execute if agent tries to modify protected files
- Layer 1: Flag and require confirmation for destructive operations
- Layer 2: Review loop (already exists)

---

## Pattern 6: Contract Net Protocol

### Concept

From FIPA (1980s). A structured negotiation protocol for task allocation:

```
1. ANNOUNCE  → Manager broadcasts task to potential contractors
2. BID       → Contractors evaluate and submit bids (capability + cost)
3. AWARD     → Manager selects best bid, awards contract
4. EXECUTE   → Contractor performs task
5. REPORT    → Contractor reports completion/failure
```

**Key properties**:
- Explicit negotiation phase before execution
- Contractors can reject tasks they can't handle
- Manager has complete information for optimal allocation
- Natural failure handling — re-announce if contractor fails

### LLM Implementations

- **MetaGPT**: roles bid on tasks based on their specialization. Product Manager proposes tasks, Engineers bid on implementation.
- **CAMEL (Communicative Agents for Mind Exploration of Large Language Models)**: two agents negotiate roles and task allocation through structured conversation.
- **ChatDev**: agents negotiate implementation details before coding begins.

### Feasibility for Harness: HIGH

Contract Net maps naturally to Harness's task lifecycle:

```
Harness Task Lifecycle        Contract Net
─────────────────────         ────────────
POST /tasks (create)     →    ANNOUNCE
Sprint planner assigns   →    BID (implicit — planner decides)
Agent picks up task      →    AWARD
Agent executes          →    EXECUTE
Review loop             →    REPORT + EVALUATE
```

**Implementation path**: Make the "BID" phase explicit:
- Before assigning a task, query agent capabilities/availability
- Allow tasks to specify required capabilities (e.g., "needs Rust expertise")
- Support re-assignment if agent fails or is overloaded
- Track contractor (agent) performance for future allocation decisions

---

## Pattern 7: Cognitive Architecture (ACT-R / SOAR)

### Concept

Models agent behavior on human cognitive processes. Key mechanisms:
- **Impasse detection**: recognize when current approach isn't working
- **Chunking**: combine frequently-used patterns into single actions
- **Episodic memory**: recall past similar situations and their outcomes
- **Goal stack**: track hierarchical goals and subgoals

```
┌──────────────────────────────────────┐
│           Working Memory              │
│  Current goal, active context,       │
│  recent observations                 │
├──────────────┬───────────────────────┤
│ Procedural   │ Declarative Memory    │
│ Memory       │ ┌──────────────────┐  │
│ (skills,     │ │ Episodic: past   │  │
│  procedures) │ │ situations       │  │
│              │ ├──────────────────┤  │
│              │ │ Semantic: facts, │  │
│              │ │ relationships    │  │
│              │ └──────────────────┘  │
├──────────────┴───────────────────────┤
│          Impasse Detection            │
│  "Am I making progress? Should I     │
│   try a different approach?"          │
└──────────────────────────────────────┘
```

### LLM Implementations

- **CoALA (Cognitive Architectures for Language Agents)**: formal framework mapping cognitive architecture to LLM agents. Working memory = context window, long-term memory = external storage, decision-making = LLM inference.
- **MemGPT/Letta**: implements virtual memory management for LLM agents. Working memory (context) + long-term storage (database), with explicit memory management operations.
- **Voyager (Minecraft agent)**: episodic memory of past successful strategies, skill library (procedural memory), automatic curriculum (goal stack).
- **Reflexion**: impasse detection via self-reflection on failed attempts.

### Feasibility for Harness: HIGH (selectively)

Not the full cognitive architecture, but specific mechanisms are highly valuable:

#### Impasse Detection

```
Agent has been running for 10 turns with no progress on tests
→ Impasse detected
→ Options: (a) try different approach (b) escalate to human (c) skip subtask
```

Currently, Harness's review loop catches quality issues post-hoc. Impasse detection would catch stuck agents in real-time.

#### Episodic Memory

```
Task: "Fix authentication bug in API"
→ Search episodic memory: "Last time auth bug was in middleware validation"
→ Inject relevant past experience into agent prompt
```

Harness already has `events.jsonl` — this could serve as episodic memory if indexed and searchable.

**Implementation path**:
1. Add impasse detection: monitor agent progress metrics (files changed, tests passing)
2. Add episodic memory: index past task events, retrieve similar for new tasks
3. Add goal stack: track task/subtask hierarchy with progress indicators

---

## Pattern 8: Event Sourcing / CQRS

### Concept

Instead of storing current state, store the sequence of events that produced the state. Current state is derived by replaying events. Commands (writes) are separated from queries (reads).

```
Events (append-only log):
  [TaskCreated, AgentSpawned, FileEdited, TestRun, ReviewRequested, ...]

State (derived):
  task.status = "in_review" (computed from events)
  task.files_changed = ["src/main.rs"] (computed from events)
  task.test_results = {passed: 5, failed: 1} (computed from events)
```

**Key properties**:
- Complete audit trail — every state change is recorded
- Time travel — reconstruct state at any point in history
- Event-driven coordination — agents react to events, not state polling
- Natural decoupling — producers and consumers of events are independent

### LLM Implementations

- **Temporal**: workflow execution as event history. Activities produce events, workflow state is derived from event replay. (See doc 06 for deep dive.)
- **Inngest**: event-driven workflow engine. Functions triggered by events, step execution recorded as events.
- **Harness events.jsonl**: already an event log. Currently used for auditing, not for state derivation or coordination.
- **OpenHands**: event-stream architecture where agent actions and observations are events in a stream.

### Feasibility for Harness: VERY HIGH

Harness already has the foundation:

```
Current:
  events.jsonl → append-only log (audit only)
  tasks.db → current state (source of truth)

Event-Sourced:
  events.jsonl → append-only log (source of truth)
  tasks.db → materialized view (derived from events)
  agents → subscribe to event stream (reactive coordination)
```

**Implementation path**:
1. Make events the source of truth (not SQLite state)
2. Derive task state from event replay
3. Allow tasks to subscribe to event patterns (e.g., "when task-1 completes, start task-2")
4. Add event-driven coordination between tasks

This is the second highest-value pattern (after HTN) because it formalizes what Harness already partially does.

---

## Recommended Combination for Harness

After evaluating all eight patterns, the recommended combination is:

```
┌────────────────────────────────────────────────────────┐
│                 EVENT SOURCING (Pattern 8)              │
│  Foundation: all state changes as events                │
│  events.jsonl → source of truth → derived state        │
├────────────────────────────────────────────────────────┤
│              HTN (Pattern 4)                            │
│  Task decomposition: goals → subtasks → actions         │
│  Sprint planner formalized as HTN planner              │
├──────────────────┬─────────────────────────────────────┤
│  BLACKBOARD (1)  │  COGNITIVE IMPASSE DETECTION (7)    │
│  Shared state    │  Monitor agent progress              │
│  for task        │  Detect stuck agents                 │
│  coordination    │  Trigger re-planning or escalation   │
└──────────────────┴─────────────────────────────────────┘
```

### Why This Combination

1. **Event Sourcing** provides the infrastructure layer — reliable state management, audit trail, and event-driven coordination
2. **HTN** provides the planning layer — structured decomposition with preconditions and re-planning
3. **Blackboard** provides the coordination layer — shared state that tasks can read from and react to
4. **Cognitive impasse detection** provides the resilience layer — catch stuck agents before they waste resources

### What to Skip

- **Market-based** (Pattern 3): over-engineered for single-orchestrator system
- **Subsumption** (Pattern 5): safety layers are valuable but don't need the full architecture
- **Contract Net** (Pattern 6): implicit in HTN + task lifecycle, doesn't need separate protocol
- **Stigmergy** (Pattern 2): already happening naturally via git, no need to formalize further
