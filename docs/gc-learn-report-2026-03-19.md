# GC Learn Report — 2026-03-19

## Overview

This report summarizes findings from the first full GC → Signal Detection → Draft → Learn cycle run on the harness project. The GC agent analyzed the event store (`events.db`) containing telemetry from 174 tasks across multiple sessions, detected 4 signals, and produced 2 actionable remediation drafts.

### Pipeline Execution Summary

| Stage | Status | Details |
|-------|--------|---------|
| Event Store | 174 tasks | `events.db` + `events.jsonl` |
| Signal Detection | 4 signals | 2x ChronicBlock, 1x RepeatedWarn, 1x WarnEscalation |
| Draft Generation | 4 drafts | 2 with content, 2 exceeded $0.50 budget |
| Adopt | 2 adopted | `661c7687` (ChronicBlock), `8626511e` (WarnEscalation) |
| Learn/Rules | Blocked | Agent resource contention; manual extraction below |

---

## Signal 1: ChronicBlock — 83 False-Positive Task Failures

**Draft ID:** `661c7687-c92a-4c6d-bc16-ff663f64e9ab`
**Signal Type:** `chronic_block`
**Remediation Type:** Rule

### Root Cause

83 task failures correlate directly with 83 guard rules — each rule triggered at least one hard block. The harness project is Rust-only, so most blocks originate from Rust guards + common coding-style rules.

### High-False-Positive Rules

#### 1. RS-03: `.unwrap()` in non-test code (~25 blocks, ~30%)

**Problem:** The guard catches all `.unwrap()` in production code, but harness has legitimate uses:
- `fn main()` entry points — exit-on-failure is correct behavior
- `Mutex::lock().unwrap()` — panicking on poisoned lock is intentional
- Test fixtures in non-`#[cfg(test)]` helper modules

**Recommended Fix:**
```
Exceptions:
- fn main() scope
- .lock().unwrap() on Mutex
- .write().unwrap() / .read().unwrap() on RwLock
```

#### 2. L1: Search-before-create (~20 blocks, ~24%)

**Problem:** `pre-write-guard.sh` blocks file creation even for new modules that by definition don't exist yet (new crate `mod.rs`, `lib.rs`, `main.rs`).

**Recommended Fix:**
```
Exempt standard Rust module files: src/**/{mod,lib,main}.rs
```

#### 3. RS-13: Action function without state effect (~15 blocks, ~18%)

**Problem:** AWK heuristics flag functions named `process_*`, `handle_*`, `execute_*` as requiring state side effects. Many are pure transformers that return `Result<T>`.

**Recommended Fix:**
```
Only flag functions that return () or Result<()>.
Functions returning typed values are transformers, not action functions.
```

#### 4. U-16: 800-line file limit (~10 blocks, ~12%)

**Problem:** Harness has legitimately large files:
- `prompts.rs` — prompt templates are inherently long
- `dispatch.rs` — exhaustive match arms across many variants

**Recommended Fix:**
```
Per-project overrides:
- **/prompts.rs: 1200-line limit
- **/dispatch.rs: 1000-line limit
```

#### 5. gh/git bash guard + CLAUDE.md double-blocking (~8 blocks, ~10%)

**Problem:** `CLAUDE.md` bans `Command::new("gh")` / `Command::new("git")` (semantic rule for agent prompts). `pre-bash-guard.sh` also mechanically blocks `gh`/`git` commands. Double-blocking causes `task_failure` on legitimate test commands.

**Recommended Fix:**
```
Deduplicate: CLAUDE.md is the semantic rule (agent prompts).
Bash guard should exempt cargo test subprocesses that invoke git for versioning.
```

### Impact Projection

| Rule | Action | Expected Reduction |
|------|--------|-------------------|
| RS-03 | Exempt `fn main()` + Mutex lock unwraps | ~25 blocks |
| L1 | Exempt standard Rust module files | ~20 blocks |
| RS-13 | Gate on return type `()` only | ~15 blocks |
| U-16 | Add project-level file size overrides | ~10 blocks |
| bash-guard + gh/git | Deduplicate enforcement | ~8 blocks |
| **Total** | | **~78/83 blocks eliminated (~94%)** |

---

## Signal 2: WarnEscalation — Warning Fatigue Pattern

**Draft ID:** `8626511e-cc47-4c01-ab13-cab991d48044`
**Signal Type:** `warn_escalation`
**Remediation Type:** Rule

### Warning Trend Analysis

| Metric | Value | Interpretation |
|--------|-------|----------------|
| First-half warn rate | 49.6% | Moderate compliance early in sessions |
| Second-half warn rate | 98.5% | Near-total warning saturation late in sessions |
| Ratio | 1.99x | Warning fatigue — agent stops heeding warns as sessions grow |

**Pattern:** Warnings are effective at session start, then degrade to noise. The 1.99x ratio (not higher) suggests a **single rule category** dominates the second-half violations. Most likely candidate: **RS-10** (silent Result discard) — common in async glue code written under time pressure late in sessions.

### Upgrade Recommendations: warn → block

#### Tier 1: Immediate (Security + Data Integrity)

| Rule | Current | Reason to block |
|------|---------|-----------------|
| **SEC-02** Hardcoded secrets | warn | Already in L7 of CLAUDE.md but not enforced mechanically — should be a hard stop before any commit |
| **RS-10** Silent `let _ = result` | warn (high) | Silently swallowed errors cause data loss; no legitimate use case in harness |
| **SEC-01** SQL/command injection | warn (critical) | Correctness-critical; zero tolerance |

#### Tier 2: High-Value Architectural Blocks

| Rule | Current | Reason to block |
|------|---------|-----------------|
| **RS-12** Dual systems same responsibility | warn (high) | Architectural rot accumulates silently; each occurrence doubles maintenance burden |
| **RS-13** Action fn without state side effect | warn (high) | Functions that claim to act but don't write state are logic bugs, not style |
| **U-23** Silent downgrade | strict (warn) | Was being bypassed — needs hard enforcement |

#### Tier 3: Keep as warn (False-Positive Risk)

| Rule | Reason to stay warn |
|------|---------------------|
| RS-03 (unwrap in non-test) | CLI entrypoints legitimately panic-on-misconfiguration |
| RS-08 (unnecessary clone) | Performance hint, not correctness |
| U-16 (file size limit) | During active refactoring this triggers constantly |
| TASTE-* | Code taste, zero correctness impact |

### Proposed Block Rules

```markdown
## BLOCK-01: Silent Result discard (RS-10 escalated)
`let _ = <Result>` or `.ok()` without log — HARD STOP.
Fix: use `if let Err(e) = ... { tracing::warn!(...) }`.

## BLOCK-02: Hardcoded credentials (SEC-02 escalated)
Any literal that looks like a token/key/password — HARD STOP.
Fix: use env vars or config file references only.

## BLOCK-03: Action function without state write (RS-13 escalated)
A function named mark_*/create_*/delete_* that returns String/()
but writes nothing — HARD STOP.
Fix: function must write state (insert/update/remove) or emit event.
```

---

## Actionable Changes

### 1. CLAUDE.md — Add VibeGuard Overrides

```markdown
## VibeGuard Overrides (Harness-specific)

- RS-03 exempt: `fn main()`, `Mutex::lock().unwrap()`, `RwLock` variants
- U-16 exempt: `prompts.rs` → 1200 lines, `dispatch.rs` → 1000 lines
- L1 exempt: new files matching `src/**/{mod,lib,main}.rs`
- RS-13: only flag `-> ()` or `-> Result<()>` functions, not typed returns
```

### 2. Guard Scripts — Update Detection Logic

| Guard | Change |
|-------|--------|
| `pre-write-guard.sh` | Exempt `mod.rs`, `lib.rs`, `main.rs` from search-before-create |
| `pre-bash-guard.sh` | Exempt `cargo test` subprocesses from gh/git ban |
| RS-03 guard | Add allowlist for `fn main()` and `Mutex/RwLock` lock patterns |
| RS-13 guard | Check return type — only flag `-> ()` and `-> Result<()>` |

### 3. Rule Severity — Escalate to Block

| Rule | From | To | Rationale |
|------|------|----|-----------|
| RS-10 | warn | block | Silent error discard = data loss |
| SEC-02 | warn | block | Hardcoded secrets = security incident |
| SEC-01 | warn | block | Injection = critical vulnerability |
| RS-12 | warn | block | Dual systems = architectural rot |
| RS-13 | warn | block | Action without effect = logic bug |
| U-23 | warn | block | Silent downgrade = hidden failure |

### 4. GC Budget — Increase for Future Runs

2 of 4 drafts failed with `Error: Exceeded USD budget (0.5)`. Recommend increasing `budget_per_signal_usd` from `0.5` to `1.0` in `config/default.toml` for more complete analysis.

---

## Learn Pipeline Observations

### What Worked
- Signal detection correctly identified chronic_block and warn_escalation patterns
- Draft content quality was high — specific, actionable, with quantified impact projections
- The GC → Draft → Adopt flow is functional end-to-end

### What Needs Improvement
1. **`learn_rules` RPC is synchronous and blocking** — times out when agent resources are contended. Should be async with a task ID like `gc_adopt`.
2. **Budget too low** — $0.50 per signal is insufficient for complex analysis. 2/4 drafts were truncated.
3. **No draft deduplication** — running GC multiple times produces duplicate signals for the same underlying issue.
4. **Adopt tasks produce no visible output** — `status=done` with `pr_url=null` gives no feedback on what was actually fixed.

---

## Appendix: Draft File Inventory

| Draft ID | Signal | Status | Content |
|----------|--------|--------|---------|
| `661c7687` | chronic_block | adopted | 83 false-positive block analysis |
| `8626511e` | warn_escalation | adopted | Warning fatigue 2x ratio analysis |
| `83367556` | repeated_warn | pending | Exceeded budget — incomplete |
| `af088d98` | chronic_block | pending | Exceeded budget — incomplete |
| `61ed1bbe` | repeated_warn | pending | Generated during second GC run |
