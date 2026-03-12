# Periodic Codebase Review

> Design document for the agent-driven periodic review mechanism.

## Problem

AI parallel development produces architectural debt that no single PR review catches.
Each agent sees only its own branch. Patterns like duplicate types, dead code, and
declaration-execution gaps only become visible at the whole-codebase level.

Harness development data (396 commits in 8 days, fix:feat ratio 131:77, 24 PRs merged
in a single day) demonstrates that per-PR reviews are insufficient for maintaining
codebase health under high-velocity AI-driven development.

## Solution

A scheduled review job that:

1. Runs daily (configurable interval)
2. Skips if no new commits since last review
3. Spawns an agent (Claude) to review the entire codebase
4. Agent produces a structured report with severity-ranked findings
5. Results stored via existing task pipeline (TaskStore)

No static analysis code. The agent IS the checker.

## Architecture

```
Scheduler::start()
  ├─ spawn gc_loop          (existing)
  ├─ spawn health_loop      (existing)
  └─ spawn review_loop      (NEW)
       │
       ├─ git log --since=<last_review> → any new commits?
       │   no  → skip, sleep
       │   yes → continue
       │
       ├─ gather context (repo structure, diff stat, recent commits)
       │
       ├─ build review prompt (11-item checklist)
       │
       └─ enqueue_task(CreateTaskRequest { prompt, source: "periodic_review" })
            └─ existing pipeline: agent → TaskState → EventStore
```

## Review Checklist (11 Items)

The review agent checks for 11 categories of issues, including 4 that are specific
to AI-generated codebases (marked with *).

### CRITICAL

#### 1. Duplicate Type Definitions

Structs or enums with the same name defined in multiple crates. Types that should
be shared from a single source (e.g., `harness-core`). Config structs duplicated
across crate boundaries.

**Why it happens:** Parallel agents scaffold independent crates without visibility
into what other agents have defined. Day 1 skeleton creates types before shared
modules stabilize.

**Example:** `GcConfig` defined in both `harness-core/config.rs` (7 fields) and
`harness-gc/gc_agent.rs` (3 fields, incomplete).

#### 7. Declaration-Execution Gap *

Components built but never wired into the actual execution path. Modules
registered in `lib.rs` but never imported or called. Functions or traits implemented
but never invoked from startup/runtime code.

**Why it happens:** Agent A builds a component. Agent B should integrate it but has
no context about Agent A's work. The component compiles, tests pass in isolation, but
the feature is non-functional end-to-end.

**Detection:** Check `build_app_state()`, `main()`, and startup code to verify all
declared components are actually connected.

**Example:** `SkillStore` implements persistence, but `discover()` is never called.

**Note:** Config structs using `Default::default()` instead of loaded values belong
to #11 (Config-Default Divergence), not here. This item covers component wiring only.

### HIGH

#### 2. Oversized Files

Any `.rs` file exceeding 400 lines. Report exact line count and suggest split points.

**Why it happens:** Each PR adds 50–100 lines. No single PR triggers a "too large"
warning. Accumulates over 10+ PRs to 1000+ lines.

**Example:** `commands.rs` (1342 lines) — 5+ subcommands in one file.

#### 3. God Objects

Structs with more than 10 public fields. Modules that mix unrelated concerns.

**Why it happens:** Each feature PR adds 1–2 fields to an existing struct. No one
questions whether the struct should be split because each addition is small.

**Example:** `AppState` with 16 public fields mixing stores, databases, notification
channels, and flags.

#### 8. Dead Code *

`pub` functions with zero call sites outside their own module. Structs or enums
defined but never instantiated. Entire modules exported via `pub mod` but never
imported by any other crate. Helper functions written speculatively but never used.

**Why it happens:** AI agents write helpers "in case they're needed." Multi-round
fix cycles leave behind functions from earlier attempts.

**Note:** `#[cfg(test)]` code is excluded from this check.

#### 10. Project Rule Violations *

Verify that `CLAUDE.md` rules are respected throughout the codebase:

- ZERO `Command::new("gh")` or `Command::new("git")` calls — all git/GitHub
  interaction must be in agent prompts only
- All user-facing strings, comments, and docs in English
- No hardcoded ports, URLs, or credentials
- `cargo fmt` compliance

**Why it happens:** Agents don't always read or follow CLAUDE.md. Rules established
after initial scaffolding may not be retroactively applied.

### MEDIUM

#### 4. Public API Leakage

`lib.rs` files that export too many `pub mod` (threshold: 5). Internal implementation
details exposed as public.

**Why it happens:** `pub mod` compiles. `pub(crate)` requires refactoring the
consumer. Agents choose the path of least resistance.

**Example:** `harness-server/lib.rs` exports 19 internal modules. CLI imports
`harness_server::thread_manager::ThreadManager` directly.

#### 5. Repeated Patterns

Same function signature pattern appearing 3+ times across files. Boilerplate that
should be abstracted (e.g., error wrapping, validation, CRUD).

**Why it happens:** Parallel PRs each create similar implementations. The "third
repetition" rule (extract on 3rd occurrence) requires cross-PR visibility that
agents lack.

**Example:** `task_db.rs`, `thread_db.rs`, `plan_db.rs` — identical
`open/insert/update/get/list` pattern.

#### 9. Error Handling Inconsistency

Mixing `anyhow::Result` and custom error types without clear boundary rules.
`let _ = result` or `.ok()` silently discarding meaningful errors. `.unwrap()` in
non-test async code where `?` should be used. Inconsistent error wrapping patterns
across handlers.

#### 11. Config-Default Divergence *

Config struct has fields with serde defaults, but consuming code constructs via
`Default::default()` instead of loading from file. The config field exists
(suggesting it was designed to be configurable) but is never actually read from
configuration sources.

**Why it happens:** Agent A defines a config struct. Agent B writes the consuming
code but constructs the config directly instead of reading it. The config field
looks like it works but user values are silently ignored.

**Detection:** For each Config struct, trace whether `load_config()` result is
actually propagated to the code that uses it.

### LOW

#### 6. Dependency Issues

Crates depending on too many workspace siblings (threshold: 5). Circular or
unnecessary dependencies.

**Example:** `harness-cli` depends on all 9 workspace crates.

## Configuration

```toml
# harness.toml
[review]
enabled = true
interval_hours = 24
# agent = "claude"      # optional: specific agent for review
# timeout_secs = 900    # optional: per-turn timeout
```

```rust
pub struct ReviewConfig {
    pub enabled: bool,           // default: false
    pub interval_hours: u64,     // default: 24
    pub agent: Option<String>,   // default: None (use default agent)
    pub timeout_secs: u64,       // default: 900
}
```

## Skip Logic

Before spawning the review agent:

1. Query EventStore for most recent `periodic_review` event → get timestamp
2. Run `git log --oneline --since=<timestamp> -1`
3. If no output → no new commits → skip this cycle
4. If no prior review event exists (first boot) → always run

## Report Format

The agent outputs a structured markdown report:

```markdown
## [SEVERITY] Category: Short Title

**File:** path/to/file.rs:LINE
**Details:** What the issue is and why it matters
**Action:** Specific fix recommendation
```

Ending with a summary table:

```markdown
| Severity | Count |
|----------|-------|
| CRITICAL | N     |
| HIGH     | N     |
| MEDIUM   | N     |
| LOW      | N     |
```

## Data Flow

```
review_loop tick
  → check EventStore for last "periodic_review" timestamp
  → git log --since → skip if empty
  → gather context (repo structure, diff stat, recent commits)
  → prompts::periodic_review_prompt() → build prompt
  → enqueue_task() → existing task pipeline
      → agent runs → report in TaskState.rounds[0].result
      → TaskStatus::Done
  → EventStore.log("periodic_review") → timestamp for next skip
```

Report is stored in `TaskState.rounds[0].result`. Query via `GET /tasks/{id}`.
Dashboard can filter by `source == "periodic_review"` for review history.

## Implementation

| File | Change | Lines |
|------|--------|-------|
| `harness-core/src/config.rs` | Add `ReviewConfig` struct + defaults | +25 |
| `harness-core/src/prompts.rs` | Add `periodic_review_prompt()` | +80 |
| `harness-server/src/periodic_reviewer.rs` | New: skip logic + context + spawn | ~120 |
| `harness-server/src/scheduler.rs` | Add review spawn in `start()` | +5 |
| `harness-server/src/lib.rs` | `pub mod periodic_reviewer;` | +1 |
| Tests | Config, skip logic, prompt | ~80 |

**Total: ~310 lines. 1 new file, 4 modified.**
