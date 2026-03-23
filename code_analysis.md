---
source: remem.save_memory
saved_at: 2026-03-20T11:14:01.239243+00:00
project: harness
---

# Complete Analysis: validate_project_root, HOME_LOCK, and AppState Structure

## Summary of Key Components

### 1. validate_project_root Function

**Location**: `/Users/apple/Desktop/code/AI/tool/harness/crates/harness-server/src/handlers/mod.rs:59-79`

**Current Implementation**:
```rust
pub(crate) fn validate_project_root(path: &std::path::Path) -> Result<std::path::PathBuf, String> {
    let home = std::env::var("HOME")
        .map(std::path::PathBuf::from)
        .map_err(|_| "HOME environment variable not set".to_string())?;
    let canonical = path
        .canonicalize()
        .map_err(|e| format!("invalid project root '{}': {e}", path.display()))?;
    if !canonical.is_dir() {
        return Err(format!(
            "project root is not a directory: {}",
            canonical.display()
        ));
    }
    if !canonical.starts_with(&home) {
        return Err(format!(
            "project root must be within HOME: {}",
            canonical.display()
        ));
    }
    Ok(canonical)
}
```

**Key Issue**: Reads `$HOME` environment variable at runtime, which causes races in tests when multiple tests temporarily change `HOME`.

### 2. validate_root! Macro

**Location**: `/Users/apple/Desktop/code/AI/tool/harness/crates/harness-server/src/handlers/mod.rs:23-35`

Used 11 times in handler functions (not counting test examples):
1. `handlers/cross_review.rs` - cross_review handler
2. `handlers/rules.rs` - rule_load handler
3. `handlers/rules.rs` - rule_check handler
4. `handlers/exec.rs` - exec handler
5. `handlers/health.rs` - health_check handler
6. `handlers/thread.rs` - thread_start handler (uses validate_root! on `cwd` instead of `project_root`)
7. `handlers/preflight.rs` - preflight handler
8. `handlers/learn.rs` - learn handler (appears twice: once in fn learn, once in fn learn_unlearn)
9. `handlers/learn.rs` - learn_unlearn handler
10. `handlers/observe.rs` - observe handler

### 3. HOME_LOCK Structure and Usage

**Location**: `/Users/apple/Desktop/code/AI/tool/harness/crates/harness-server/src/test_helpers.rs:12`

```rust
pub static HOME_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
```

**Purpose**: Serializes all tests that read or mutate the process-global `HOME` environment variable. Without this lock, tests that temporarily change `HOME` race with any other test that calls `validate_project_root()` (which reads `HOME`).

**Usage Pattern**:
```rust
let _lock = HOME_LOCK.lock().await;
let dir = tempdir_in_home("prefix-")?;  // Creates tempdir under $HOME
let state = make_test_state(dir.path()).await?;
// ... handler calls validate_project_root() which reads $HOME
```

**Used in 30+ test locations** across:
- `/crates/harness-server/src/http.rs` (3 locations)
- `/crates/harness-server/src/http/tests.rs` (2 locations)
- `/crates/harness-server/src/stdio.rs` (2 locations)
- `/crates/harness-server/src/task_runner.rs` (4 locations)
- `/crates/harness-server/src/handlers/rules.rs` (3 locations)
- `/crates/harness-server/src/handlers/health.rs` (2 locations)
- `/crates/harness-server/src/handlers/dashboard.rs` (2 locations)
- `/crates/harness-server/src/handlers/observe.rs` (4 locations)
- `/crates/harness-server/src/router/tests.rs` (10 locations)

### 4. HomeGuard RAII Pattern

**Location**: `/Users/apple/Desktop/code/AI/tool/harness/crates/harness-server/src/test_helpers.rs:14-40`

```rust
pub struct HomeGuard {
    original: Option<String>,
}

impl HomeGuard {
    /// Overwrite `HOME` with `path` and return a guard that restores it.
    /// # Safety
    /// The caller must hold `HOME_LOCK` for the lifetime of this guard.
    pub unsafe fn set(path: &std::path::Path) -> Self {
        let original = std::env::var("HOME").ok();
        std::env::set_var("HOME", path);
        HomeGuard { original }
    }
}

impl Drop for HomeGuard {
    fn drop(&mut self) {
        unsafe {
            match self.original.take() {
                Some(h) => std::env::set_var("HOME", h),
                None => std::env::remove_var("HOME"),
            }
        }
    }
}
```

Ensures `HOME` is restored on drop, even on panic.

### 5. tempdir_in_home Helper

**Location**: `/Users/apple/Desktop/code/AI/tool/harness/crates/harness-server/src/test_helpers.rs:49-62`

```rust
pub fn tempdir_in_home(prefix: &str) -> anyhow::Result<tempfile::TempDir> {
    let home = std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::current_dir().expect("resolve cwd"));
    if let Ok(dir) = tempfile::Builder::new().prefix(prefix).tempdir_in(&home) {
        return Ok(dir);
    }
    let fallback = std::env::current_dir()?.join(".harness-test-home");
    std::fs::create_dir_all(&fallback)?;
    tempfile::Builder::new()
        .prefix(prefix)
        .tempdir_in(&fallback)
        .map_err(Into::into)
}
```

Creates a temp directory under `$HOME` if writable, otherwise falls back to `.harness-test-home` in `cwd`.

### 6. AppState Structure

**Location**: `/Users/apple/Desktop/code/AI/tool/harness/crates/harness-server/src/http.rs:123-142`

```rust
pub struct AppState {
    pub core: CoreServices,        // project_root stored here
    pub engines: EngineServices,
    pub observability: ObservabilityServices,
    pub concurrency: ConcurrencyServices,
    pub notifications: NotificationServices,
    pub intake: IntakeServices,
    pub interceptors: Vec<Arc<dyn harness_core::interceptor::TurnInterceptor>>,
    pub project_svc: Arc<dyn crate::services::ProjectService>,
    pub task_svc: Arc<dyn crate::services::TaskService>,
    pub execution_svc: Arc<dyn crate::services::ExecutionService>,
}
```

**CoreServices contains**:
```rust
pub struct CoreServices {
    pub server: Arc<HarnessServer>,
    pub project_root: std::path::PathBuf,  // ← Project root stored here
    pub tasks: Arc<task_runner::TaskStore>,
    pub thread_db: Option<crate::thread_db::ThreadDb>,
    pub plan_db: Option<crate::plan_db::PlanDb>,
    pub plan_cache: Arc<DashMap<String, harness_exec::ExecPlan>>,
    pub project_registry: Option<std::sync::Arc<crate::project_registry::ProjectRegistry>>,
}
```

### 7. Test State Creation

**Location**: `/Users/apple/Desktop/code/AI/tool/harness/crates/harness-server/src/test_helpers.rs:64-179`

Key functions:
- `make_test_state(dir)` - creates AppState with both data and project root pointing to same directory
- `make_test_state_with_project_root(dir, project_root)` - creates AppState with separate data and project root directories
- `make_state_inner(dir, project_root, agent_registry)` - implementation detail

The `project_root` is stored in `AppState.core.project_root` during initialization.

### 8. Project Registry validate_project_root

**Location**: `/Users/apple/Desktop/code/AI/tool/harness/crates/harness-server/src/project_registry.rs:80-93`

```rust
pub fn validate_project_root(root: &std::path::Path) -> Result<(), String> {
    if !root.is_dir() {
        return Err(format!("root is not a directory: {}", root.display()));
    }
    if !root.join(".git").exists() {
        return Err(format!(
            "root is not a git repository (no .git found): {}",
            root.display()
        ));
    }
    Ok(())
}
```

**Note**: This is a different function with the same name! Used in `handlers/projects.rs:44` for HTTP project registration. Validates git repository instead of HOME containment.

### 9. Historical Context (Recent Commits)

Recent commits addressing HOME/tilde issues:
- `bdbdf40` (Mar 16): "expand tilde in intake data_dir and extend HOME_LOCK to all tempdir_in_home tests"
  - serve(): pass expand_tilde(data_dir) to build_orchestrator
  - Add HOME_LOCK to handlers/observe.rs, handlers/health.rs, stdio.rs tests
  
- `46fcf81`: "remove HOME_LOCK double-acquire deadlocks in tests"
- `c4a43bd`: "address reviewer issues in default.toml.example and HOME race"
- `4c771c7`: "acquire HOME_LOCK in all tests that read or write HOME env var"

## Key Observations for Refactoring

1. **HOME is read at validation time** - validate_project_root() calls `std::env::var("HOME")` at runtime, not at AppState creation time

2. **AppState already contains project_root** - stored in `AppState.core.project_root` as a PathBuf

3. **Could pass home_dir via AppState** - instead of reading from env in validate_project_root, could accept it as parameter or read from AppState

4. **Two different validate_project_root functions exist**:
   - `handlers/mod.rs` - checks containment within HOME (HOME-aware)
   - `project_registry.rs` - checks for git repo (HOME-agnostic)

5. **Test pattern is well-established**:
   - Lock HOME_LOCK before calling tempdir_in_home()
   - Create state with that directory
   - Call handlers which invoke validate_root! macro
   - Lock ensures no concurrent HOME mutations

6. **11 handler call sites use validate_root! macro** - all in handlers directory, returning early with INTERNAL_ERROR on failure

## Potential Solutions

### Option A: Pass home_dir as parameter to validate_project_root
- Requires adding home_dir parameter to function
- 11 call sites in macros would need updating
- Tests can still use HOME_LOCK without contention on env var reads

### Option B: Store home_dir in AppState
- Add `home_dir: PathBuf` field to AppState or CoreServices
- Pass state to validate_project_root or make it a method
- Eliminates env var read at runtime
- Requires thread-local or Arc to access from macro context

### Option C: Use thread-local home_dir
- Store home_dir in thread-local storage during test setup
- validate_project_root reads thread-local instead of env var
- Minimally invasive to call sites
- Still requires HOME_LOCK in tests for safety

### Option D: Read home_dir once at server startup
- Store in HarnessServer or GlobalConfig
- All validation calls use the startup value
- Production-ready since server starts with stable HOME
- Tests need careful fixture setup

