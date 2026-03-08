# Harness vs OpenAI App Server 差距分析（2026-03-06）

## 1. 评估范围与基线

- 仓库：`/Users/lifcc/Desktop/code/AI/tools/harness`
- 对比基线：OpenAI 公开的 Codex App Server（`openai/codex`）
- 说明：`openai/harness` 公开仓库不可访问，本报告用 OpenAI 官方公开 App Server 文档与 README 作为能力基线。

参考：
- https://github.com/openai/codex/blob/main/codex-rs/app-server/README.md
- https://developers.openai.com/codex/app-server/

## 2. 总体结论

- 若仅看本仓库“自定义协议方法是否都接线”，完成度较高（`Method` 37 个均可路由）。
- 若看“与 OpenAI App Server 的真实兼容和运行时能力”，当前仍有明显差距。
- 综合估计：还差约 **50%-60%**（能力维度）。

## 3. 高优先级差距（建议优先修复）

### H1 协议方法体系不兼容 OpenAI 风格

- 现状：本仓库方法名为 `snake_case`（如 `thread_start`）。
- 基线：OpenAI App Server 使用 slash 风格（如 `thread/start`、`turn/start`）。
- 证据：
  - `crates/harness-protocol/src/methods.rs:9`
  - `crates/harness-protocol/src/methods.rs:18`
  - `crates/harness-protocol/src/methods.rs:26`

### H2 `turn/start` 未驱动真实 agent 执行

- 现状：`turn_start` 只更新 thread/turn 状态并发通知，不执行 agent。
- 影响：无法形成与 OpenAI 类似的 turn 生命周期（started -> item deltas -> completed）。
- 证据：
  - `crates/harness-server/src/handlers/thread.rs:109`
  - `crates/harness-server/src/handlers/thread.rs:123`

### H3 HTTP/WebSocket 通知链路未闭环

- 现状：
  - HTTP 入口未挂载 `/ws`。
  - handler 通过 `notify_tx` 发通知。
  - websocket 监听的是 `notification_tx`。
- 影响：HTTP+WS 模式下 server-push 不能形成单一、可验证的通知通路。
- 证据：
  - `crates/harness-server/src/http.rs:185`
  - `crates/harness-server/src/handlers/thread.rs:64`
  - `crates/harness-server/src/websocket.rs:108`

### H4 多 agent 调度未进入主执行路径

- 现状：
  - `AgentRegistry` 已有 `dispatch(TaskClassification)`。
  - 主路径仍固定使用 `default_agent()`。
  - `serve` 启动仅注册 `claude`。
- 影响：复杂任务无法根据分类进行动态 agent 选择。
- 证据：
  - `crates/harness-agents/src/registry.rs:30`
  - `crates/harness-server/src/http.rs:221`
  - `crates/harness-cli/src/commands.rs:234`

### H5 规则引擎默认无 guard，扫描常为“空违规”

- 现状：
  - `scan()` 只执行已注册 guard 脚本。
  - 启动流程仅加载 rules 文档，未注册 guards。
- 影响：规则系统“可声明但弱执行”，对 runtime 约束不足。
- 证据：
  - `crates/harness-rules/src/engine.rs:244`
  - `crates/harness-server/src/http.rs:92`

## 4. 中优先级差距

### M1 Skill 持久化发现链未打通

- 现状：
  - `with_persist_dir()` 已配置。
  - 但未调用 `discover()`，无法自动回读 repo/user/admin 层技能。
- 证据：
  - `crates/harness-server/src/http.rs:141`
  - `crates/harness-skills/src/store.rs:58`

### M2 流式输出仍是“整段后发”

- 现状：`execute_stream()` 内部先 `execute()`，再一次性发送完成 item。
- 影响：缺少真正 token/item delta streaming。
- 证据：
  - `crates/harness-agents/src/claude.rs:80`
  - `crates/harness-agents/src/codex.rs:63`

### M3 GC 产物解析与 adopt 路径仍偏 stub

- 现状：
  - `parse_artifacts()` 把全部输出塞成单文件草稿。
  - `adopt()` 直接写 `artifact.target_path`。
- 影响：GC 产物结构化不足，路径与落盘安全边界需继续加强。
- 证据：
  - `crates/harness-gc/src/gc_agent.rs:194`
  - `crates/harness-gc/src/gc_agent.rs:121`

### M4 ExecPlan 仅内存态

- 现状：plan 在 `AppState.plans` 内存 map 管理。
- 影响：重启后丢失，跨会话追踪能力弱。
- 证据：
  - `crates/harness-server/src/handlers/exec.rs:19`
  - `crates/harness-server/src/http.rs:148`

## 5. 已经对齐或明显进展点

- `Method` 到 handler 的路由已全接（不再是早期大面积 `METHOD_NOT_FOUND`）。
  - 证据：`crates/harness-server/src/router.rs:9`
- 线程与任务有 SQLite 持久化基础。
  - 证据：`crates/harness-server/src/thread_db.rs:8`
  - 证据：`crates/harness-server/src/task_runner.rs:246`
- 拦截器链已接入 task 执行路径。
  - 证据：`crates/harness-server/src/task_executor.rs:23`
  - 证据：`crates/harness-server/src/http.rs:150`

## 6. 验证记录（本地）

- 命令：`cargo test -p harness-server --quiet`
  - 结果：通过（86+1 tests）
- 命令：`cargo test --workspace --quiet`
  - 结果：通过（workspace 全量通过）

## 7. 建议的三步路线（高 ROI）

1. 统一通知总线并正式挂载 `/ws` 路由，打通 HTTP+WS 通知闭环。
2. 让 `turn/start` 进入真实 agent 执行与流式 item 事件输出。
3. 增加 OpenAI 协议兼容层（方法名映射、握手约束、turn 语义对齐）。

## 8. Issue 化 TODO 清单（可直接开工）

### Issue 1: 协议兼容层（`snake_case` -> slash 风格）

- Priority: P0
- Goal: 支持 OpenAI 风格方法名（如 `thread/start`），并保留现有 `snake_case` 兼容。
- Files:
  - `crates/harness-protocol/src/methods.rs`
  - `crates/harness-protocol/src/codec.rs`
  - `crates/harness-server/src/router.rs`
- Acceptance:
  - `initialize`、`initialized`、`thread/start`、`turn/start` 可正常收发。
  - 旧方法名（如 `thread_start`）不回归（至少一个版本兼容）。
  - 未初始化时返回明确错误（Not initialized / Already initialized 语义）。
- Verification:
  - `cargo test -p harness-protocol --quiet`
  - `cargo test -p harness-server --quiet`

### Issue 2: HTTP 挂载 WebSocket + 通知总线统一

- Priority: P0
- Goal: HTTP 模式下 `/ws` 可用，并且 thread/turn 通知能稳定推送到 WS 客户端。
- Files:
  - `crates/harness-server/src/http.rs`
  - `crates/harness-server/src/websocket.rs`
  - `crates/harness-server/src/notify.rs`
  - `crates/harness-server/src/handlers/thread.rs`
- Acceptance:
  - `/ws` 路由已挂载并通过 Origin 校验。
  - handler 产生的通知能进入 websocket 订阅通道。
  - 压测下 lag 统计可观测，丢弃行为可预期。
- Verification:
  - `cargo test -p harness-server websocket --quiet`
  - 增加一个 HTTP+WS 端到端通知测试（thread_start -> ws 收到事件）。

### Issue 3: `turn/start` 驱动真实 agent 生命周期

- Priority: P0
- Goal: `turn/start` 不只是写状态，要真正触发 agent 执行并写回 turn items / turn status。
- Files:
  - `crates/harness-server/src/handlers/thread.rs`
  - `crates/harness-server/src/task_executor.rs`
  - `crates/harness-server/src/thread_manager.rs`
- Acceptance:
  - `turn/start` 后 turn 状态从 running -> completed/failed/cancelled。
  - thread 中记录 agent 输出 item、token usage。
  - `turn/status` 能看到执行后状态与输出。
- Verification:
  - 新增 `turn_start_executes_agent_and_completes_turn` 集成测试。
  - `cargo test -p harness-server --quiet`

### Issue 4: 真流式输出（delta/item 级别）

- Priority: P1
- Goal: `execute_stream` 改为真实流式，不再“整段执行完成后一次性回填”。
- Files:
  - `crates/harness-agents/src/claude.rs`
  - `crates/harness-agents/src/codex.rs`
  - `crates/harness-core/src/agent.rs`
  - `crates/harness-protocol/src/notifications.rs`
- Acceptance:
  - 支持 agent message delta（或等价的 chunk item）逐步下发。
  - turn 结束前可看到多条中间通知。
  - 断连/超时路径无 panic，状态可恢复。
- Verification:
  - `cargo test -p harness-agents --quiet`
  - 增加至少 1 个流式通知顺序测试。

### Issue 5: 多 agent 调度接入主路径

- Priority: P1
- Goal: 任务执行调用 `AgentRegistry::dispatch`，并在 `serve` 注册 `codex` / `anthropic-api`。
- Files:
  - `crates/harness-server/src/http.rs`
  - `crates/harness-server/src/task_runner.rs`
  - `crates/harness-cli/src/commands.rs`
  - `crates/harness-server/src/complexity_router.rs`
- Acceptance:
  - 复杂/关键任务优先走 `claude`（或策略指定 agent）。
  - 简单任务可走默认 agent。
  - 调度策略可通过测试验证。
- Verification:
  - `cargo test -p harness-server --quiet`
  - 新增 dispatch 选择行为测试。

### Issue 6: Guard 自动装载与规则执行闭环

- Priority: P1
- Goal: 启动时自动注册 guard 脚本，让 `rule_check` 默认可产出违规。
- Files:
  - `crates/harness-rules/src/engine.rs`
  - `crates/harness-server/src/http.rs`
  - `rules/`（新增 guard 脚本目录约定）
- Acceptance:
  - 无需手工 `register_guard` 也能执行至少一组默认 guard。
  - `rule_check` 结果与 event 持久化一致。
  - guard 失败时错误可观测，不吞异常。
- Verification:
  - `cargo test -p harness-rules --quiet`
  - `cargo test -p harness-server --quiet`

### Issue 7: Skill 发现链与 trigger_patterns 生效

- Priority: P1
- Goal: 启动时执行 `discover()`，并解析 skill frontmatter 的 `trigger_patterns`。
- Files:
  - `crates/harness-skills/src/store.rs`
  - `crates/harness-server/src/http.rs`
  - `crates/harness-server/src/task_executor.rs`
- Acceptance:
  - 重启后 repo/user/admin/system 技能可见。
  - `match_context()` 能基于路径/语言命中技能。
  - 技能注入可从“全量注入”优化为“按上下文注入”。
- Verification:
  - `cargo test -p harness-skills --quiet`
  - `cargo test -p harness-server --quiet`

### Issue 8: ExecPlan 持久化到 SQLite

- Priority: P2
- Goal: `ExecPlanInit/Update/Status` 不再仅内存态，支持重启恢复。
- Files:
  - `crates/harness-server/src/handlers/exec.rs`
  - `crates/harness-server/src/http.rs`
  - `crates/harness-server/src`（新增 `plan_db.rs`）
- Acceptance:
  - plan 可持久化、查询、更新、重启后恢复。
  - 旧内存路径平滑迁移，不破坏现有 RPC。
- Verification:
  - 新增 `exec_plan_persist_roundtrip` 测试。
  - `cargo test -p harness-server --quiet`

### Issue 9: JSON-RPC 错误码语义化

- Priority: P2
- Goal: 区分 not found / invalid params / internal error，避免全部 `INTERNAL_ERROR`。
- Files:
  - `crates/harness-server/src/handlers/*.rs`
  - `crates/harness-protocol/src/methods.rs`
- Acceptance:
  - 参数错误返回 `-32602`。
  - 方法未找到返回 `-32601`。
  - 业务不存在（如 thread/skill/draft 不存在）使用约定业务码或明确错误 data。
- Verification:
  - 为每类错误至少补 1 个集成测试。

### Issue 10: project_root 安全策略可配置化

- Priority: P2
- Goal: 将“必须在 HOME 下”的限制改为可配置 allowlist，兼容 CI/workspace 场景。
- Files:
  - `crates/harness-server/src/handlers/mod.rs`
  - `crates/harness-core/src/config.rs`
  - `config/default.toml`
- Acceptance:
  - 默认安全策略不降低。
  - 可通过配置增加合法工作区根目录。
  - 拒绝越界路径的行为保持一致。
- Verification:
  - 新增策略配置测试（HOME 内/外、allowlist 命中/未命中）。

