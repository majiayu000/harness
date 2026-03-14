---
source: remem.save_memory
saved_at: 2026-03-14T01:41:16.610271+00:00
project: manual
---

# Harness Multi-Project Architecture Research Summary

# Harness Multi-Project Architecture Research

## Research Date
2026-03-14

## Findings Summary

### 1. Server Startup: Single Project Binding
- **CLI**: `harness serve --project-root <path>` sets ONE project root at startup
- **Config**: ServerConfig.project_root is loaded from:
  - CLI flag `--project-root` (optional)
  - Config file `server.project_root` field (defaults to current working directory)
  - Once set, it's stored in AppState.core.project_root and becomes immutable for the server's lifetime
- **Location**: crates/harness-cli/src/commands/serve.rs:27-28
- **Limitation**: Cannot switch projects without restarting the server

### 2. Task Creation (POST /tasks) 
- **Request Format** (CreateTaskRequest):
  - `prompt`: Optional free-text description
  - `issue`: Optional GitHub issue number
  - `pr`: Optional GitHub PR number
  - `agent`: Optional agent name (defaults to dispatcher)
  - `project`: Optional PathBuf for project root (NEW extension point)
  - `wait_secs`: Polling interval (default 120s)
  - `max_rounds`: Review loop iterations (default 5)
  - `turn_timeout_secs`: Per-turn timeout (default 3600s)
  - `max_budget_usd`: Spend limit (optional, unlimited by default)
  - `stall_timeout_secs`: Silence detection timeout (default 300s)
  - `source`: Intake source name (github, feishu, etc.)

- **Current Behavior**: 
  - If req.project is None, it's overwritten with state.core.project_root (HTTP fallback at line 849)
  - For intake webhooks (GitHub, Feishu), req.project is None → defaults to server's single project_root
  - Location: crates/harness-server/src/http.rs:848-849

### 3. Project Resolution in Tasks
- **Entry Point**: task_runner::spawn_task() → spawn_task_with_worktree_detector()
- **Logic** (task_runner.rs:435-474):
  - If req.project is provided: use it directly
  - If req.project is None: spawn_blocking(detect_main_worktree) to find git main worktree
  - Validates project_root via validate_project_root()
- **Worktree Detection**: Uses `git worktree list --porcelain` to find main worktree root
- **Fallback**: Returns "." if detection fails with warning logged

### 4. Workspace/Worktree Management
- **WorkspaceManager**: Optional per-server component
- **Behavior**: 
  - If enabled via config: creates isolated git worktree per task for parallel execution
  - Location: crates/harness-server/src/task_runner.rs:554-565
  - Worktree root: `config.server.workspace.root` (default: ~/.local/share/harness/workspaces)
  - Automatic cleanup when task reaches Done/Failed if `auto_cleanup: true`
- **Cleanup**: task_runner.rs:582-588

### 5. Task Queue & Concurrency
- **Queue Implementation**: crates/harness-server/src/task_queue.rs
- **Concurrency Config** (crates/harness-core/src/config/misc.rs):
  - `max_concurrent_tasks`: Default 4 (global limit across all projects)
  - `max_queue_size`: Default 32 (max waiting tasks before rejection)
  - `stall_timeout_secs`: Default 300 (silence detection)
- **Mechanism**: Semaphore-based with atomic queue counter
- **Backpressure**: Returns error immediately if queue is full
- **Per-Task Permit**: Released automatically on task completion (or panic)

### 6. Task Status Monitoring
- **Polling (Pull Model)**:
  - `GET /tasks/{task_id}` → returns TaskState snapshot
  - `GET /tasks` → lists all tasks (lightweight TaskSummary format)
  - No push/webhook model for task status changes
- **Streaming**:
  - `GET /tasks/{task_id}/stream` → Server-Sent Events (SSE) for real-time agent output
  - Broadcast channel per task (capacity: 512 items, backpressure: oldest dropped)
  - Location: task_runner.rs:12-14, 306-376

### 7. Scheduler & Background Jobs
- **Scheduler**: crates/harness-server/src/scheduler.rs
- **Tasks**:
  - Periodic GC runs (interval based on codebase quality grade: 1h-7d)
  - Health tick every 24h (rule scan + quality reporting)
  - Periodic reviewer (if enabled in config)
- **Scope**: All tasks use the single server project_root for rule scanning

### 8. Multi-Project Limitations

**Hard Constraints**:
1. **Single Project Root per Server**: Must restart server to change project
2. **Global Task Queue**: Concurrency limits apply across all projects (if multi-project were added)
3. **Server-Level Configuration**: Rules, skills, agents loaded once at startup from single project_root
4. **Workspace Isolation**: Worktrees are scoped to single project (separate file trees per task within that project)

**Soft Extension Points**:
1. CreateTaskRequest.project field: Can override per-task (already in API!)
2. detect_main_worktree: Could be parameterized by project
3. WorkspaceManager: Could be extended to track project-specific workspaces
4. Rule scanning: Currently uses server.project_root in scheduler; could be per-project

### 9. Key Code Locations

**Single-Project Binding**:
- CLI setup: crates/harness-cli/src/commands/serve.rs:27-28
- AppState initialization: crates/harness-server/src/http.rs:158-240
- Project fallback in requests: crates/harness-server/src/http.rs:848-850

**Task Creation & Execution**:
- CreateTaskRequest: crates/harness-server/src/task_runner.rs:141-177
- spawn_task: crates/harness-server/src/task_runner.rs:405-620
- Project resolution: crates/harness-server/src/task_runner.rs:253-267, 435-474

**Workspace Management**:
- WorkspaceConfig: crates/harness-core/src/config/misc.rs:7-52
- Task worktree creation: crates/harness-server/src/task_runner.rs:554-565
- Cleanup: crates/harness-server/src/task_runner.rs:582-588

**Queue & Concurrency**:
- TaskQueue: crates/harness-server/src/task_queue.rs
- ConcurrencyConfig: crates/harness-core/src/config/misc.rs:54-91

**Scheduler**:
- Scheduler loop: crates/harness-server/src/scheduler.rs:21-45
- Health tick: crates/harness-server/src/scheduler.rs:47-79

**Server Endpoints**:
- POST /tasks: crates/harness-server/src/http/task_routes.rs:84-107
- GET /tasks: crates/harness-server/src/http.rs:696 (route registration)

## Extension Strategy for Multi-Project Support

### Minimal Changes (API-Level Only)
1. Trust CreateTaskRequest.project field (already exists, currently overwritten)
2. Remove fallback at http.rs:848-849 for explicit project requests
3. Validate project is within allowed set (policy layer)

### Medium Effort (Project Registry)
1. Add ProjectRegistry to AppState
2. Keep server.project_root as "primary", allow secondary projects
3. Validate all task projects against registry
4. Update rule scanning scheduler to handle multiple projects
5. Extend WorkspaceManager to support project-specific workspace roots

### High Effort (True Multi-Project Server)
1. Eliminate global project_root from ServerConfig
2. Require project field in all task requests
3. Make all per-project components (rules, skills, agents) configurable per-project
4. Split task queue by project or add project-aware scheduling
5. Update scheduler to scan all registered projects

## Current State Assessment

**Architecture**: Single-project-per-server is intentional and well-integrated
- Simplifies initialization, configuration, rules loading, and state management
- WorkspaceManager provides isolated execution within one project
- Parallel dispatch already splits complex tasks into subtasks (same project)
- Queue is bounded globally, not per-project

**API Readiness**: 60% ready for multi-project
- CreateTaskRequest.project field already exists
- Task executor accepts project override
- Missing: project validation, registry, scheduler support

