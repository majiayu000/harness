# Harness 向 DeerFlow 学习改造 TodoList（2026-03-25）

## 1. 目标

- 在不做向后兼容的前提下，把 `harness` 从“强内核”推进到“可直接上手的产品化编排平台”。
- 本轮只做 4 件事：
  - 一键启动与环境自检（DX）
  - 扩展注册中心（skills/tools/MCP 的统一启停与元数据）
  - 只读 Web 控制台（任务/线程/事件可视化）
  - 文档与示例路径标准化

## 2. 事实 / 推断 / 决策

### 事实

- `harness` 当前强项是 Rust 内核治理与 CI 质量门禁。
- `deer-flow` 当前强项是产品化入口（配置、启动、可视化、扩展管理）。

### 推断

- `harness` 当前瓶颈主要在“可用性外壳”，不是核心执行能力。

### 决策

- 本次改造默认允许 breaking change，不保留旧配置字段、旧命令别名、旧 API 路径。

## 3. 范围与非目标

### 范围（必须完成）

- 统一启动入口：`make doctor`、`make dev`、`make up`。
- 新增扩展中心配置：`config/extensions.toml`。
- 新增只读控制台：`/dashboard`（任务、线程、事件、队列状态）。
- 新增“5 分钟跑通”文档与示例配置。

### 非目标（本轮不做）

- 不做 UI 写操作（不在控制台里发任务/改配置）。
- 不做多租户权限系统重构。
- 不做历史 API 兼容层。

## 4. 里程碑与检查清单

## M0：基线冻结（P0）

- [ ] 建立基线文档：记录当前 CLI、HTTP 路由、配置字段全集。
- [ ] 输出 breaking change 清单（明确“删除了什么”）。
- [ ] 固化回归命令脚本（本轮每阶段重复执行）。
- [ ] 在 `docs/` 新增迁移说明入口（旧用法 -> 新用法）。

完成标准：
- [ ] 任意成员能在 10 分钟内看懂“旧接口删除范围”。

## M1：一键化开发体验（P0）

- [ ] 新增项目根 `Makefile` 目标：
  - [ ] `make doctor`：检查 Rust/cargo、agent CLI、端口占用、必要环境变量。
  - [ ] `make dev`：并行启动 `harness serve` + 控制台前端开发服务。
  - [ ] `make up`：生产模式构建并启动。
  - [ ] `make down`：停止服务并清理运行态文件。
- [ ] 在 CLI 增加 `harness doctor` 子命令（输出结构化诊断结果）。
- [ ] 端口策略落地：默认从 `5567` 起，禁止使用保留端口。
- [ ] 失败必须显式报错并带上下文（模块/动作/关键参数）。

完成标准：
- [ ] 新机器首次拉仓后，按 README 只执行 3 条命令可跑通。

## M2：扩展注册中心（P1）

- [ ] 新增 `config/extensions.toml`（单一真实来源）：
  - [ ] 扩展 ID、类型（skill/tool/mcp）、启停状态、加载参数、来源路径。
  - [ ] 支持项目级覆盖：`<project>/.harness/extensions.toml`。
- [ ] 实现扩展加载器统一入口，替换分散发现逻辑。
- [ ] 支持配置热重载（mtime 或显式 reload 命令）。
- [ ] 增加 `harness extensions list|enable|disable|reload`。
- [ ] 删除旧的重复发现路径与冗余配置分支（不保留兼容层）。

完成标准：
- [ ] 任意 skill/tool/mcp 只需在 1 处注册即可被系统识别。
- [ ] 禁用后立即生效，且状态可观测。

## M3：只读 Web 控制台（P1）

- [ ] 新建前端模块（建议 `apps/dashboard`），只做只读页面。
- [ ] 页面最小集合：
  - [ ] 任务列表（状态、项目、耗时、失败原因）。
  - [ ] 线程详情（turn 时间线、agent、token/耗时统计）。
  - [ ] 事件流（GC、规则命中、webhook、队列拥塞）。
  - [ ] 项目队列状态（全局并发 vs 项目并发）。
- [ ] 后端新增只读聚合 API（不复用内部调试接口）。
- [ ] 前后端接口写入 `docs/api-contract.md` 并加 schema 校验。
- [ ] 默认不返回假数据；无数据展示空白态。

完成标准：
- [ ] 不看日志也能定位“任务为什么卡住/失败”。

## M4：文档与示例（P1）

- [ ] 重写 README 的 Quick Start（本地 + 生产两条路径）。
- [ ] 新增 `docs/usage-quickstart-5min.md`。
- [ ] 新增 `config/harness.toml.example` 与 `config/extensions.toml.example`。
- [ ] 新增“从旧版本迁移”文档，明确删除项与替代项。
- [ ] 补齐控制台截图与最小 demo 流程。

完成标准：
- [ ] 新用户不看代码，只看文档可在 30 分钟内完成一次任务执行。

## M5：测试与 CI（P0，持续）

- [ ] Rust：每轮改动后执行 `cargo check`。
- [ ] Web/API：每轮改动后执行 `npx tsc --noEmit`（若引入前端工作区）。
- [ ] 提交前执行：
  - [ ] `cargo test --workspace`
  - [ ] 前端项目测试命令（若已建立）
- [ ] 新增端到端烟雾测试：
  - [ ] `doctor -> dev -> submit task -> dashboard 可见 -> 任务完成`
- [ ] 所有新功能必须配测试，不允许只改实现不补验证。

完成标准：
- [ ] CI 能稳定复现本地主流程，失败信息可直接定位模块。

## 5. 文件级改动清单（计划）

- [ ] 新增 [Makefile](/Users/lifcc/Desktop/code/AI/tools/harness/Makefile)
- [ ] 更新 [README.md](/Users/lifcc/Desktop/code/AI/tools/harness/README.md)
- [ ] 新增 `config/extensions.toml` 与 example 文件
- [ ] 更新 `crates/harness-cli/src/commands.rs`（doctor/extensions 命令）
- [ ] 更新 `crates/harness-core/src/config/*`（extensions 配置模型）
- [ ] 更新 `crates/harness-skills/src/*`、`crates/harness-server/src/*`（统一加载/只读 API）
- [ ] 新增 `apps/dashboard/*`（只读 UI）
- [ ] 更新 [docs/api-contract.md](/Users/lifcc/Desktop/code/AI/tools/harness/docs/api-contract.md)
- [ ] 新增 `docs/usage-quickstart-5min.md`
- [ ] 新增迁移文档：`docs/migration-2026-03-breaking.md`

## 6. 执行顺序（严格）

- [ ] 第 1 阶段：M0 + M1（先解决可用性）
- [ ] 第 2 阶段：M2（统一扩展源）
- [ ] 第 3 阶段：M3（可视化观测）
- [ ] 第 4 阶段：M4 + M5（文档收口与质量门禁）

## 7. Done-When（验收门槛）

- [ ] 仅用新文档可从零启动并跑通一次任务。
- [ ] 扩展启停能实时生效且可审计。
- [ ] 控制台能定位任务失败原因，不依赖手工翻日志。
- [ ] CI 覆盖新链路，且无向后兼容代码残留。

