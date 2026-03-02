# Harness (Rust) 落地方案（行为对齐 OpenAI Harness Engineering）

## 0. 目标与边界

- 目标: 构建独立 Rust 平台服务，复现公开可见的 Harness 工程实践。
- 复现口径: **行为对齐**，不是内部实现逐行对齐。
- 首期强约束:
  - Thread / Turn / Item 三原语
  - JSON-RPC 2.0（stdio 优先，HTTP/WebSocket 次之）
  - Claude Code + Codex 双引擎
  - GC Agent（信号检测 -> 草案 -> 人工采纳）
  - Rules + Skills + ExecPlan + Observe
- 非目标（首期不做）:
  - 多租户 SaaS 控制面
  - 全量 MCP 平台化
  - 自动合并到主分支（默认仅草案和人工采纳）

## 1. 关键架构决策（先定再写代码）

1. 控制面 / 执行面分离  
- 控制面: `harness-server`（会话、协议、调度、持久化）。  
- 执行面: `harness-agents`（CLI/API 适配器），通过统一 `CodeAgent` trait 接入。

2. 事件优先设计（Event-first）  
- 所有 turn 进展都先写事件流，再更新聚合状态。  
- Thread/Turn 是“可重建投影”，不是唯一事实源。

3. Turn 异步任务模型  
- `turn/start` 立即返回 `turn_id`，执行在后台 tokio task。  
- 状态更新靠通知流（started/item_started/item_completed/completed）。

4. 可取消语义  
- `turn/cancel` 必须幂等；已结束 turn 返回 no-op。  
- CLI agent 需要进程组 kill + 超时兜底。

5. GC 采纳必须事务化  
- adopt 采用“临时文件写入 + 校验 + 原子替换 + 回滚点”。  
- 默认 dry-run 预览 diff，人工确认后落盘。

## 2. 你当前方案的 6 个必修正点

1. `ThreadManager::resume_thread -> &Thread` 不可行  
- `DashMap` + async 下返回引用会有生命周期和锁问题。  
- 改为 `Arc<ThreadSnapshot>` 或值拷贝 DTO。

2. Item 存储会膨胀  
- `FileRead/FileEdit` 内容可能很大。  
- 需引入 item blob 表（或压缩列）+ 摘要索引列。

3. 协议版本缺失  
- `initialize` 必须协商 `protocol_version` 与 `capabilities`。  
- 后续字段演进靠版本门控。

4. 通知背压未定义  
- WebSocket/stdio 慢消费者会拖垮执行。  
- 需要 bounded queue + 丢弃策略（仅丢中间进度，不丢终态）。

5. GC 风险边界太宽  
- `allowed_tools` 不应含写操作（生成阶段只读）。  
- 写入只在 `adopt` 阶段由 server 执行。

6. Rule/Skill 发现链优先级要可审计  
- 冲突覆盖必须记录来源（repo/user/admin/system）与决策日志。

## 3. 实施路线（P0 -> P2）

## P0（第 1-2 周）可运行骨架

范围:
- workspace + 10 crates 骨架
- `harness-core` 类型 + `harness-protocol` 方法/通知
- `harness-server` 的 stdio JSON-RPC
- SQLite 持久化（thread/turn/item/event 最小表）
- `harness-cli serve --transport stdio`

完成标准:
- 可以 `thread/start -> turn/start -> turn/status`
- turn 产生 item 流通知
- 重启后能 `thread/resume`

验收命令:
- `cargo check --workspace`
- `cargo test -p harness-core -p harness-protocol -p harness-server`

## P1（第 3-5 周）Agent + Rules + Skills + ExecPlan

范围:
- `harness-agents`: ClaudeCode/Codex/AnthropicAPI 适配器
- `AgentRegistry` + 任务分发策略
- `harness-rules`: 4 层加载 + 扫描执行 + 违规输出统一格式
- `harness-skills`: 发现、去重、延迟全文加载
- `harness-exec`: ExecPlan markdown round-trip

完成标准:
- `harness exec \"list files\" --agent codex|claude` 可跑通
- `harness rule check <project>` 输出 violations
- `harness skill list/get` 正常
- `harness plan init/status/update` 正常

验收命令:
- `cargo test -p harness-agents -p harness-rules -p harness-skills -p harness-exec`

## P2（第 6-8 周）GC + Observe + HTTP/WebSocket

范围:
- `harness-gc`: signal detector + draft store + adopt/reject
- `harness-observe`: event query + quality grader
- `harness-server`: HTTP/WebSocket transport
- CLI 子命令补齐（gc/rule/skill/plan）

完成标准:
- `harness gc run` 可生成草案
- `harness gc adopt <id>` 原子写入并校验
- metrics/quality report 可查询
- WebSocket 客户端可收到流式 turn 事件

验收命令:
- `cargo test --workspace`
- 端到端集成测试通过（tests/e2e/*.rs）

## 4. 数据与协议最小落地建议

1. 数据库表先最小化  
- `threads(id,status,project_root,metadata,created_at,updated_at)`  
- `turns(id,thread_id,status,agent_id,token_in,token_out,started_at,completed_at)`  
- `items(id,turn_id,kind,summary,blob_ref,created_at)`  
- `item_blobs(id,content,encoding)`  
- `events(id,ts,session_id,thread_id,turn_id,type,payload)`

2. 协议分层  
- request/response（同步）  
- notification（异步事件）  
- error code（可机读分类）

3. 错误码建议  
- `H100x` 协议错误  
- `H200x` 持久化错误  
- `H300x` agent 调用错误  
- `H400x` rule/gc 执行错误

## 5. 风险清单（提前规避）

1. CLI 适配器不稳定  
- 做命令能力探测与版本检查；失败降级到 API agent。

2. 大项目 I/O 压力  
- 文件扫描 + 事件写入要限速和批量化。

3. GC 误修复  
- 强制 draft 审核、规则复扫、可回滚备份点。

4. 协议演进破坏兼容  
- initialize 协商版本；旧字段保留至少一个小版本周期。

## 6. “/plan open” 对应动作

- 这里不依赖 `/plan open` 命令。  
- 计划文件已落盘到:
  - `tools/harness/plan/harness-rust-platform-plan.md`
- 直接在 VS Code 打开该文件即可编辑和执行。
