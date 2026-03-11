# Harness Architectural Issues — 2026-03-12

## Root Cause: Declaration-Execution Gap Pattern

**Pattern Definition**: Harness exhibits "declaration-execution gap" where frameworks are architecturally sound but startup wiring is missing. Components are well-designed with complete isolation and unit tests, but the integration glue (startup registration, parameter threading) is incomplete or incorrect, causing features to silently degrade end-to-end.

**Historical Examples** (from memory #1888, #1890):
- **SkillStore** — `discovery_paths` config exists but `discover()` never called on startup
- **RulesConfig** — Fields loaded but consumers call `Default::default()` ignoring config
- **ThreadManager** — Carried dead fields while `AppState.thread_db` was canonical
- **GC adoption** — Received `project_root` but failed propagating it
- **PlanDb** — Had full CRUD but startup didn't call `list()` to load data (fixed in #1891)

**Meta-Pattern**: When refactoring to introduce abstraction (module, trait, config), verify the complete integration chain:
1. Declaration (code structure)
2. Execution (startup/initialization)
3. Validation (CI coverage)
4. Persistence (commit/push)

**Missing any link produces silent failures.**

---

## Discovered Issues (2026-03-12 Audit)

### HIGH PRIORITY

#### 1. Duplicate GcConfig Definition
**Files**: 
- `crates/harness-core/src/config.rs:505-530` (7 fields)
- `crates/harness-gc/src/gc_agent.rs:11-26` (3 fields)

**Impact**: Config drift — changes to core config don't propagate to gc_agent.

**Fix**: Delete duplicate in `harness-gc/gc_agent.rs`, import from `harness-core::config::GcConfig`.

---

#### 2. AppState God Object
**File**: `crates/harness-server/src/http.rs:23-53`

**Issue**: 14+ fields managing disparate concerns:
- Thread/task management (`server`, `tasks`)
- Skills/rules engines (`skills`, `rules`)
- Observability (`events`)
- GC (`gc_agent`)
- Plans (`plans`, `plan_db`)
- Workspace (`workspace_mgr`)
- Notifications (4 fields)
- Initialization (`initialized`)
- Concurrency (`task_queue`)
- Intake (`feishu_intake`)
- Interceptors

**Impact**: Difficult to test, unclear dependencies, violates SRP.

**Fix**: Group into sub-structs:
```rust
pub struct AppState {
    pub core: CoreServices,
    pub engines: EngineServices,
    pub observability: ObservabilityServices,
    pub concurrency: ConcurrencyServices,
    pub notifications: NotificationServices,
    pub interceptors: Vec<Arc<dyn TurnInterceptor>>,
}
```

---

### MEDIUM PRIORITY

#### 3. Duplicate Status Conversion Functions
**Files**:
- `crates/harness-server/src/task_db.rs:171-194`
- `crates/harness-server/src/thread_db.rs:131-148`

**Fix**: Extract to generic trait in `harness-core` or `harness-server/src/db_utils.rs`.

---

#### 4. Oversized Files (>800 lines)

| File | Lines | Should Split Into |
|------|-------|-------------------|
| `crates/harness-cli/src/commands.rs` | 1342 | Domain-specific command modules |
| `crates/harness-core/src/config.rs` | 1168 | `config/server.rs`, `config/agents.rs`, etc. |
| `crates/harness-server/src/task_executor.rs` | 1061 | `task_executor.rs` + `task_prompts.rs` + `pr_detection.rs` |
| `crates/harness-server/src/router.rs` | 917 | Use macro or builder pattern |
| `crates/harness-rules/src/engine.rs` | 798 | `engine.rs` + `rule_loader.rs` |

---

#### 5. TaskId Definition Location
**Issue**: TaskId defined in `harness-server/src/task_runner.rs:17-24` instead of `harness-core/src/types.rs` where all other IDs use `define_id!` macro.

**Fix**: Move to `harness-core/src/types.rs` and use macro pattern.

---

#### 6. Inconsistent Error Handling
**Issue**: Three separate error types with overlapping concerns:
- `harness-core/src/error.rs` — HarnessError (generic)
- `harness-sandbox/src/lib.rs:41-57` — SandboxError
- `harness-server/src/task_db.rs:126-134` — TaskDbDecodeError

Most handlers use `anyhow::Result<T>` losing error context.

**Fix**: Consolidate to single error hierarchy in `harness-core`.

---

#### 7. Repeated Prompt Building Patterns
**File**: `crates/harness-server/src/task_executor.rs:59-150`

**Issue**: Three similar functions with repeated logic:
- `build_fix_ci_prompt()`
- `build_pr_rework_prompt()`
- `build_pr_approved_prompt()`

**Fix**: Extract to generic `PromptBuilder` pattern.

---

#### 8. Missing Database Abstraction Layer
**Files**:
- `crates/harness-server/src/task_db.rs` (409 lines)
- `crates/harness-server/src/thread_db.rs` (297 lines)

Both duplicate: `open()`, `migrate()`, `insert()`, `update()`, `get()`, `list()`, `delete()`, status conversion, row deserialization.

**Fix**: Create `GenericDb<T: DbEntity>` trait.

---

### LOW PRIORITY

#### 9. Inconsistent Config Naming
**File**: `crates/harness-core/src/config.rs`

**Issue**: 20+ Config structs with mixed naming styles, some with `Default` impl, some with `#[serde(default)]`.

**Fix**: Standardize to `{Domain}Config` pattern.

---

#### 10. Repeated Validation Logic
**File**: `crates/harness-server/src/handlers/mod.rs`

**Issue**: `validate_project_root()` called 12 times with repeated error handling boilerplate.

**Fix**: Create macro to reduce boilerplate.

---

#### 11. Circular Dependency Risk
**Issue**: `harness-server` depends on 8 crates. No circular deps detected but future changes could cascade.

**Mitigation**: Consider extracting `harness-api` crate for stable interfaces.

---

#### 12. Unused Dead Code Attributes
**File**: `crates/harness-server/src/task_db.rs:120-123`

**Issue**: 2 instances of `#[allow(dead_code)]` should be removed or fields should be used.

---

## Why These Problems Exist

**Incremental development + lack of periodic refactoring**:
1. Features added one by one without holistic design review
2. No regular "architecture debt" cleanup sprints
3. CI validates individual PRs but not cross-feature integration
4. Parallel feature branches can introduce conflicts invisible to isolated CI (see memory #1892 — E0761 module ambiguity recurrence)

**Missing integration validation**: Unit tests pass, isolated integration tests pass, but combined state breaks post-merge.

---

## Recommendations

1. **Quarterly architecture review** — Identify god objects, duplicates, oversized files
2. **Pre-merge integration testing** — Validate parallel branches don't conflict
3. **Refactoring budget** — Allocate 20% of sprint capacity to debt cleanup
4. **Startup wiring checklist** — For new features, verify declaration → execution → validation → persistence chain
5. **Module naming convention** — Prevent E0761 ambiguity (prefer single `common.rs` over `common/mod.rs` for test helpers)
