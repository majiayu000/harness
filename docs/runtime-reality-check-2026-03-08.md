# Harness 设计初衷 vs 运行时现状复核（2026-03-08）

## 结论

- 结论成立：当前仓库的主要问题确实是“声明-执行鸿沟”（接口/结构存在，但运行时接线不完整）。
- 同时有两点需要修正：
  - Webhook 并非“缺测试”，已有较完整的 HTTP 集成测试；缺的是真实 GitHub 回调环境验收。
  - GC→Learn 不应定义为“代码完整”，因为规则 guard 默认未自动注册，输入信号可能长期为空，闭环有效性未被端到端证明。

## 逐项复核

| 项目 | 复核结果 | 证据 | 备注 |
|---|---|---|---|
| Agent Loop（prompt→tool→observe→iterate） | 成立 | `crates/harness-agents/src/claude.rs:38-77`, `crates/harness-agents/src/codex.rs:74-107` | 仍是单次 subprocess + 等待完整输出。 |
| 流式输出（delta 级） | 成立 | `crates/harness-agents/src/claude.rs:99-126`, `crates/harness-agents/src/codex.rs:120-138` | `execute_stream()` 先 `execute()`，再一次性发完成事件。 |
| 多 agent 调度 | 成立 | `crates/harness-agents/src/registry.rs:30-40`, `crates/harness-server/src/http/task_routes.rs:23-33` | `dispatch()` 存在，但主路径仍走 `default_agent()`。 |
| GC → Learn 闭环 | 部分成立 | `crates/harness-server/src/scheduler.rs:21-29`, `crates/harness-server/src/handlers/learn.rs`, `crates/harness-rules/src/engine.rs:289-300`, `crates/harness-server/src/http.rs:117-131` | 调度和 learn 入口存在，但 guard 未自动注册会导致 scan 可能长期为空。 |
| 规则引擎 guard 执行 | 成立 | `crates/harness-rules/src/engine.rs:285-300`, `crates/harness-server/src/http.rs:117-131` | `scan()` 依赖已注册 guard，启动流程未自动注册 guard。 |
| Skill discover() | 成立（已修复） | `crates/harness-server/src/http.rs:173-177` | 启动时已调用 `discover()`。 |
| Webhook 自动触发 | 需修正 | `crates/harness-server/src/http.rs:282-283`, `crates/harness-server/src/http/tests.rs:185-465` | Endpoint 和测试都在；仍缺“真实 GitHub webhook 环境验收”。 |
| Sandbox 隔离 | 成立 | `crates/harness-sandbox/src/lib.rs:14-19`, `crates/harness-sandbox/src/lib.rs:59-131`, `crates/harness-agents/src/claude.rs:62-68`, `crates/harness-agents/src/codex.rs:77-83` | 多引擎封装与 agent 接入都已存在。 |
| 协议兼容（slash style） | 成立 | `crates/harness-protocol/src/methods.rs:190-193`, `crates/harness-protocol/src/methods.rs:263-305` | 解码兼容 slash 名称，且 canonical method name 为 slash 风格。 |
| SDK（TypeScript/Python） | 成立 | `docs/harness-sdk.md:1-15`, `sdk/typescript/README.md:1-38`, `sdk/python/README.md:1-37` | 两套 SDK 与测试目录已存在。 |

## 记录下的 Issue 清单（可直接创建）

### 1) [P0] `turn/start` 接入真实 Agent 执行生命周期

- 目标：`turn/start` 不只创建 turn，要驱动 agent 执行并回写 turn item/status/token usage。
- 关键文件：`crates/harness-server/src/handlers/thread.rs`、`crates/harness-server/src/thread_manager.rs`、`crates/harness-server/src/task_executor.rs`
- DoD：新增集成测试覆盖 running→completed/failed/cancelled 全路径。

### 2) [P0] 统一 stdio/http/ws 通知总线，修复 WS 推送链路

- 目标：handler 产生的通知能在 HTTP+WS 模式可靠送达。
- 关键文件：`crates/harness-server/src/http.rs`、`crates/harness-server/src/websocket.rs`、`crates/harness-server/src/notify.rs`、`crates/harness-server/src/handlers/thread.rs`
- DoD：新增端到端测试 `thread_start/turn_start -> ws 收到通知`。

### 3) [P1] 多 Agent 调度接入主路径 + 注册 `anthropic-api`

- 目标：默认按复杂度调度 agent，而非固定 `default_agent()`。
- 关键文件：`crates/harness-server/src/http/task_routes.rs`、`crates/harness-server/src/complexity_router.rs`、`crates/harness-cli/src/commands.rs`、`crates/harness-cli/src/cmd/mcp_server.rs`
- DoD：复杂任务走策略路由；`anthropic-api` 可被注册并选择。

### 4) [P1] `execute_stream` 改为真流式 delta

- 目标：任务执行期间持续发增量事件，而不是结束后一次性回填。
- 关键文件：`crates/harness-agents/src/claude.rs`、`crates/harness-agents/src/codex.rs`、`crates/harness-protocol/src/notifications.rs`
- DoD：新增事件顺序测试（delta 在 done 前出现，且可中断/超时收敛）。

### 5) [P1] ExecPlan 持久化读路径补齐（重启恢复）

- 目标：`exec_plan/status` 不仅查内存 map，能回查 DB；启动时回填计划缓存。
- 关键文件：`crates/harness-server/src/handlers/exec.rs`、`crates/harness-server/src/http.rs`、`crates/harness-server/src/plan_db.rs`
- DoD：重启后 `status` 仍可读取既有 plan。

### 6) [P2] Guard 自动注册与空扫描显式告警

- 目标：启动时自动注册内置 guard；无 guard 时对 `rule/check` 和 `gc/run` 给显式告警。
- 关键文件：`crates/harness-rules/src/engine.rs`、`crates/harness-server/src/http.rs`、`crates/harness-server/src/handlers/rules.rs`
- DoD：默认规则扫描可产生可观测结果，不再“静默空违规”。

## 现阶段定位

- 该仓库不是“没做出来”，而是“中台框架已成形、关键运行时闭环未打通”。
- 因此下一阶段优先级应从“继续加模块”转为“先打通接线 + 做端到端验收”。
