# Fixflow Spec — AppState/Enqueue/Timeout/Test-Setup Refactor (2026-03-26)

## Goal
修复以下确认问题（按优先级）：#2 任务入队双实现、#5 stall_timeout_secs 配置假象、#4 测试 AppState 手工重复、#1 AppState 在 http 模块导致边界反转、#3 超大文件（先拆 http.rs）。

## Constraints
- Backward compatibility: not required
- Commit policy: per_step (blocked)
- Validation scope:
  - Step-level: `cargo check -p harness-server`
  - Final: `cargo check -p harness-server` + 关键测试子集
- Dirty baseline (pre-existing):
  - README.md
  - config/default.toml.example
  - crates/harness-agents/src/registry.rs
  - crates/harness-cli/src/cmd/pr.rs
  - crates/harness-cli/src/commands/serve.rs
  - crates/harness-cli/src/gc.rs
  - crates/harness-core/src/config.rs
  - crates/harness-core/src/config/agents.rs
  - crates/harness-server/src/dashboard.rs
  - crates/harness-server/src/handlers/mod.rs
  - crates/harness-server/src/http.rs
  - crates/harness-server/src/http/tests.rs
  - crates/harness-server/src/periodic_reviewer.rs
  - crates/harness-server/static/dashboard.css
  - crates/harness-server/static/dashboard.js
  - docs/usage-guide.md
  - crates/harness-server/src/handlers/token_usage.rs (untracked)
  - docs/exploration/ (untracked dir)
  - docs/todo/ (untracked dir)
  - plan/spec-codex-full-chain-2026-03-25.md (untracked)
- Blockers:
  - 目标文件（如 `http.rs`、`http/tests.rs`）存在预先改动；若强制 per-step 提交会混入非本轮改动，先以“代码+验证”交付。

## Steps
1. Introduce app_state module
   - done when: `AppState/CoreServices/...` 与 `resolve_reviewer` 从 `http.rs` 迁出，非 http 模块不再依赖 `http::AppState`。
2. Unify enqueue path
   - done when: `create_task` 与 batch 背景入队都走 `execution_svc`，`task_routes` 不再维护完整入队复制逻辑。
3. Remove dead stall timeout field
   - done when: `CreateTaskRequest.stall_timeout_secs` 删除，所有调用/测试构造同步。
4. Consolidate test state construction
   - done when: `router/tests.rs`、`stdio.rs`、`websocket.rs`、`http/tests.rs` 不再手写大段 `AppState { ... }`。
5. Split http responsibilities
   - done when: `http.rs` 至少拆出独立职责子模块（auth / inbound），主文件明显瘦身。
6. Verify
   - done when: 指定检查全部通过，失败项有根因和修复记录。

## Risks
- `AppState` 迁移会触发跨模块编译连锁。
- 入队逻辑收敛后，batch 的冲突串行语义必须保持。
- 测试 helper 收敛可能改变初始化细节（需保持 initialized 标志语义）。
