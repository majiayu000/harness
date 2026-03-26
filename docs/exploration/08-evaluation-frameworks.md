# Evaluation Frameworks for Agent Systems

## 1. SWE-bench Ecosystem

SWE-bench is the de facto standard for evaluating coding agents, but it has evolved into a family of related benchmarks.

### Variants

| Variant | Size | Description | Status |
|---------|------|-------------|--------|
| **SWE-bench Full** | 2,294 | Original dataset from 12 Python repos | Contamination concerns |
| **SWE-bench Lite** | 300 | Curated subset, cleaner tasks | Widely used but saturating |
| **SWE-bench Verified** | 500 | Human-verified solvable, clear specifications | Most reliable, recommended |
| **SWE-bench Pro** | ~1,000 | Harder tasks, multi-file changes | Newer, less saturated |
| **SWE-bench Multimodal** | 600+ | Tasks with visual components (UI bugs, charts) | Early stage |

### How It Works

```
Input: GitHub issue description + repository snapshot
Expected: Patch that resolves the issue
Evaluation: Apply patch → run repo's test suite → check previously-failing tests now pass
```

### Contamination Issues

SWE-bench Full has known contamination problems:
- Some tasks appear in training data of newer models
- Task descriptions sometimes leak the solution approach
- Repository snapshots may include hints in commit history
- Some "gold patches" are overly specific (testing one solution approach)

**SWE-bench Verified** addresses most of these by having humans verify:
1. The issue is clearly specified
2. The gold patch is correct
3. The tests adequately validate the fix
4. Multiple solution approaches would be accepted

### Current Scores (as of early 2026)

| System | SWE-bench Verified | Architecture |
|--------|-------------------|--------------|
| Claude Code (Opus 4) | ~72% | Single agent, nO loop |
| Codex (GPT-4.1) | ~69% | Single agent, ReAct |
| Cursor Agent | ~65% | Multi-agent (Planner/Worker) |
| Devin | ~55% | Multi-agent (4 agents) |
| Aider (Opus) | ~48% | Single agent, architect/editor |
| SWE-Agent | ~40% | Single agent, ACI |
| Agentless | ~32% | No agent, pipeline |

Note: Scores are approximate and change frequently. Verified is the most stable benchmark.

### Saturation Warning

SWE-bench Lite is approaching saturation — top systems score >70%, making it harder to differentiate improvements. The field is moving toward harder benchmarks (Pro, Multimodal) and task-specific evaluations.

---

## 2. Terminal-Bench

### Focus

Tests **tool selection and sequencing**, not just code patching. Tasks require agents to:
- Choose appropriate terminal commands
- Sequence multi-step operations correctly
- Handle command output and errors
- Navigate file systems effectively

### Key Difference from SWE-bench

SWE-bench tests: "Can you write the right patch?"
Terminal-Bench tests: "Can you figure out what to do and do it?"

### Task Examples

- "Find all Python files that import both `os` and `sys` and add a logging import"
- "Set up a new virtual environment, install dependencies from requirements.txt, and run the test suite"
- "Diagnose why the build is failing and fix it"

### Why It Matters

Many agent failures in practice aren't about code quality — they're about wrong tool selection, incorrect command sequences, or failure to handle unexpected output. Terminal-Bench specifically targets these orchestration-level skills.

---

## 3. MultiAgentBench

### Focus

Evaluates multi-agent **collaboration dynamics**, not just individual agent capability.

### Metrics

| Metric | What It Measures |
|--------|-----------------|
| **Task completion rate** | Did the multi-agent system solve the task? |
| **Communication efficiency** | Tokens spent on inter-agent communication vs. productive work |
| **Handoff success rate** | % of agent transitions that preserved necessary context |
| **Coordination overhead** | Extra time/cost from multi-agent vs. single-agent |
| **Error propagation** | How often does one agent's error cascade to others? |
| **Recovery rate** | Can the system recover from individual agent failures? |

### Task Types

- **Parallel independent**: tasks that can be split with no dependencies
- **Sequential dependent**: tasks where output of one agent feeds next
- **Negotiation**: tasks requiring agents to agree on approach
- **Adversarial**: one agent tries to subvert another (red team/blue team)

### Key Finding

Multi-agent systems typically spend **20-40% of total tokens on coordination** (inter-agent messages, context transfer, status updates). Systems spending >50% on coordination are net-negative compared to single-agent.

---

## 4. GAIA (General AI Assistants)

### Focus

Multi-step planning and execution for general-purpose tasks. Requires combining multiple skills (web search, calculation, reasoning, file handling).

### Difficulty Levels

| Level | Description | Example |
|-------|-------------|---------|
| **Level 1** | 1-3 steps, single skill | "What year was X founded?" |
| **Level 2** | 3-7 steps, multiple skills | "Compare the GDP of these 5 countries in 2023 and rank them" |
| **Level 3** | 7+ steps, complex reasoning | "Analyze this dataset, generate insights, create a visualization" |

### Used By

- Magentic-One (~38%)
- OpenHands-Versa (64.24%)
- Various research systems

### Why It Matters for Orchestration

GAIA Level 3 tasks are where multi-agent systems theoretically shine — complex enough to benefit from specialization. But OpenHands-Versa (single agent) beating Magentic-One (multi-agent) on GAIA challenges this assumption.

---

## 5. SWE-Lancer

### Focus

Real freelancer tasks with **economic value**. Tasks are sourced from actual freelancing platforms with known dollar values.

### Key Innovation

Tasks include **ambiguity** — like real client requests:
- Vague specifications ("make it look better")
- Contradictory requirements ("fast AND cheap AND perfect")
- Scope creep potential ("while you're at it, also fix...")

### Why It Matters

SWE-bench tasks are well-specified (clear issue, clear tests). Real-world tasks are ambiguous. SWE-Lancer tests whether agents can handle ambiguity through:
- Asking clarifying questions
- Making reasonable assumptions
- Delivering partial solutions when full solution is unclear
- Estimating effort and communicating tradeoffs

### Dollar Values

Tasks have known freelancer pricing ($50-$5000), enabling cost-effectiveness analysis:
- "Agent solved a $500 task for $2 in compute" = high value
- "Agent spent $50 in compute on a $50 task" = break-even
- "Agent spent $200 in compute and failed a $100 task" = negative value

---

## 6. Orchestration-Specific Metrics

No existing benchmark specifically isolates orchestration quality from agent capability. Here are the metrics that would be needed:

### Coordination Overhead Ratio

```
Coordination Overhead = (Total tokens - Productive tokens) / Total tokens

Where:
  Total tokens = all tokens consumed by all agents
  Productive tokens = tokens directly contributing to task output
  Coordination tokens = inter-agent messages, planning, status updates, context transfer
```

| Range | Assessment |
|-------|------------|
| **5-15%** | Ideal — minimal coordination cost |
| **15-25%** | Acceptable — some overhead is expected |
| **25-30%** | Warning — consider simplifying architecture |
| **>30%** | Over-orchestration — multi-agent is likely hurting |

### Handoff Success Rate

```
Handoff Success Rate = Successful handoffs / Total handoffs

Successful handoff = receiving agent has sufficient context to continue without:
  - Asking for clarification
  - Repeating work already done
  - Making errors from missing context
```

Target: >90% for production systems.

### Planning Accuracy

```
Planning Accuracy = (Tasks completed as planned) / (Total planned tasks)

Measures: Does the planner's decomposition actually work?
  - Missed dependencies → planning failure
  - Unnecessary tasks → over-planning
  - Wrong task ordering → re-planning needed
```

### Parallelization Efficiency

```
Parallelization Efficiency = Sequential time / Actual parallel time

Perfect parallelization: N tasks in time of 1 task (efficiency = N)
Real-world: coordination overhead reduces efficiency

Example:
  4 tasks, each takes 10 min sequentially = 40 min
  4 tasks in parallel, actual time = 15 min
  Efficiency = 40/15 = 2.67x (vs theoretical 4x)
```

---

## 7. LLM-as-Judge

Using LLMs to evaluate other LLMs' outputs. Increasingly common for subjective quality assessment.

### Approaches

#### Direct Scoring

```
Prompt: "Rate this code review on a scale of 1-10 for:
  - Accuracy of identified issues
  - Completeness of coverage
  - Quality of suggestions
  - Appropriate tone"

Output: {accuracy: 8, completeness: 6, suggestions: 7, tone: 9}
```

#### Comparative Evaluation (Pairwise)

```
Prompt: "Which code review is better, A or B? Consider accuracy, completeness, and actionability."

Output: "Review A is better because... [reasoning]"
```

#### Rubric-Based

```
Prompt: "Evaluate this agent's performance against these criteria:
  1. Did it correctly identify the root cause? (0-3 points)
  2. Did it modify only the necessary files? (0-2 points)
  3. Did the fix include a test? (0-2 points)
  4. Did it explain the fix clearly? (0-1 point)
  5. Did it introduce any regressions? (-3 per regression)"
```

### Trajectory Grading

Instead of evaluating just the final output, evaluate the agent's **trajectory** (sequence of actions):

```
Turn 1: Agent searches for relevant files → GOOD (correct starting point)
Turn 2: Agent reads wrong file → SUBOPTIMAL (could have been more targeted)
Turn 3: Agent realizes mistake, reads correct file → GOOD (self-correction)
Turn 4: Agent makes correct fix → GOOD
Turn 5: Agent runs tests → GOOD
Turn 6: Agent runs tests again unnecessarily → SUBOPTIMAL (wasted turn)

Trajectory score: 4/6 optimal decisions = 67%
```

### Known Biases

| Bias | Description | Mitigation |
|------|-------------|------------|
| **Self-preference** | Models prefer their own outputs | Use different model as judge |
| **Position bias** | First option in A/B comparison preferred | Randomize presentation order |
| **Length bias** | Longer responses rated higher | Penalize unnecessary verbosity |
| **Sycophancy** | Judge agrees with confident statements | Use rubric-based evaluation |
| **Style bias** | Preference for certain writing styles | Focus rubric on substance, not style |

### Best Practices

1. **Use a different model as judge** than the one being evaluated
2. **Randomize presentation order** for comparative evaluations
3. **Use rubrics** to reduce subjectivity
4. **Multi-judge** panels (3+ evaluations, take majority)
5. **Calibrate** against human evaluations on a subset
6. **Track inter-judge agreement** (Cohen's kappa > 0.6)

---

## 8. The Missing Benchmark: Orchestration Quality

### The Gap

Current benchmarks conflate orchestration quality with agent capability:

```
High SWE-bench score = Good agent + Good orchestration (can't tell which)
Low SWE-bench score = Bad agent? Bad orchestration? Both?
```

### What's Needed

A benchmark that holds agent capability constant and varies orchestration:

```
Same model, same tools, different orchestration strategies:
  Strategy A: Single agent, no planning
  Strategy B: Single agent, with planning step
  Strategy C: Multi-agent, orchestrator + specialist
  Strategy D: Multi-agent, peer-to-peer

Measure: task completion, cost, time, error rate
```

### Proposed Dimensions

1. **Task decomposition quality**: given a complex task, how well does the orchestrator break it down?
2. **Agent selection**: does the orchestrator choose the right agent/model for each subtask?
3. **Context transfer**: does critical information survive agent transitions?
4. **Error recovery**: when an agent fails, does the orchestrator recover gracefully?
5. **Resource efficiency**: does the orchestrator minimize unnecessary work?

### Why This Doesn't Exist Yet

- Hard to control for agent capability across different models
- Orchestration and agent quality are deeply intertwined
- The field is still debating what "good orchestration" means
- Commercial incentives favor end-to-end benchmarks (more marketing-friendly)

---

## 9. Implications for Harness

### Evaluation Strategy

| What to Evaluate | Benchmark/Method | Priority |
|---|---|---|
| Agent task completion | SWE-bench Verified (if applicable) | Medium |
| Orchestration overhead | Custom: coordination token ratio | High |
| Review loop quality | LLM-as-judge on review accuracy | High |
| Planning accuracy | Track sprint_planner predictions vs. outcomes | High |
| End-to-end cost | Dollar cost per resolved task | High |

### Recommended Metrics to Track

1. **Cost per resolved task** (dollars)
2. **Time per resolved task** (wall clock)
3. **Review loop iterations** (how many review cycles before LGTM)
4. **Planning accuracy** (% of sprint_planner tasks that succeed as planned)
5. **Agent turn efficiency** (productive turns / total turns)

These metrics can be computed from Harness's existing `events.jsonl` and `tasks.db` — no new infrastructure needed.
