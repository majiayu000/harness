# Tech Spec

## Linked Issue

GH-1434

## Product Spec

`specs/GH1434/product.md`

## Codebase Context

| Area | Files | Current behavior | Why relevant |
| --- | --- | --- | --- |
| thread/turn RPC | `crates/harness-protocol/src`（thread/* ×8、turn/* ×8 方法定义）、`crates/harness-server/src/router/mod.rs`（注册）、`handlers/thread.rs` | 完整生命周期 RPC，历史仅 9 threads | 删除主体 1 |
| thread 状态 | `crates/harness-server/src/thread_manager.rs`、`thread_db.rs`、`thread_manager_tests.rs` | 自有 thread 状态机 + Postgres（thread_db.threads） | 删除主体 1 |
| task 层 | `crates/harness-server/src/task_db{,.rs}`、`task_executor/`、`task_runner/`、`task_queue_tests.rs` | 第二代任务状态机（task_db.tasks，62 行数据） | 删除主体 2；先做依赖扫描，runtime 仍引用的部分并入 |
| eval | `crates/harness-eval`（848 LOC）、`eval_store` schema、workspace Cargo.toml | 0 eval_runs | 删除主体 3 |
| review_store | `crates/harness-server/src/review_store/`、`review_store` schema | 0 findings | 删除主体 4 |
| 消费方 | `crates/harness-cli/src/main.rs`、`crates/harness-server/src/dashboard.rs`、`websocket.rs`、`web/` | 可能引用 thread/task 端点 | 先改消费方后删端点（product 边界情况） |
| 探针落点 | router 分发处 + 各模块公开入口 | 无使用计数 | T001 |
| 事实来源 | workflow runtime（`workflow_runtime_*`、`crates/harness-workflow`） | 全部真实流量 | 保留面，行为不变的基准 |

## Proposed Design

**探针（先行 PR）**：轻量计数器 `UsageProbe`——`static AtomicU64` per surface +
每日一条 info 事件汇总（`probe_report` 事件：surface → count）。覆盖两层：
(a) router 里 thread/*、turn/* 方法分发点；(b) `thread_manager`、`task_db`、
`task_executor`、`task_runner`、`harness-eval`、`review_store` 的公开函数入口
（宏或手工，取实现简单者）。探针本身无配置、无持久化，7 天后读事件即可。

**归档**：`scripts/archive-phase1-data.sh`——对 `thread_db.threads`、
`task_db.tasks` 及外键关联表、各 hash schema 中同名表执行
`pg_dump --table` 到 `archives/phase1-<date>/`，附 `RESTORE.md`（恢复命令）。
表保留原地不动（本阶段不 drop、不 rename，避免运行中代码踩空）。

**删除顺序**（每步独立 PR，依赖从少到多）：
1. `harness-eval`：从 workspace members 移除 crate；删 `evaluate.py` 中的
   引用（如有）；eval_store schema 留档不动。
2. `review_store`：删模块目录 + router/handlers 引用。
3. thread/turn：先删 CLI/dashboard/websocket 消费方 → 删 handlers/thread.rs、
   router 注册 → 删 protocol 方法定义 → 删 thread_manager/thread_db。
4. task 层：`cargo tree`/`rg` 依赖扫描出 workflow runtime 对 task_* 的真实
   引用清单；被引用的类型/函数移入 `workflow_runtime_*` 邻近模块（仅搬运，
   不改语义），其余删除。

**RPC 面收缩后**：protocol 保留 skill/*（Phase 3 处理）、gc/*（Phase 3）、
rule/*、metrics/*、event/*、stats、task/classify（归属 runtime，改名或保留）、
health。thread/turn 16 个方法移除进 CHANGELOG breaking 清单。

## Product-to-Test Mapping

| Product invariant | Implementation area | Verification |
| --- | --- | --- |
| P1 探针先行 + removal PR 引用计数 | UsageProbe + PR 模板纪律 | 探针单测 + PR 描述人工核对 |
| P2 归档先于删除 | archive 脚本 + RESTORE.md | 脚本跑通、dump 文件存在、恢复演练一次 |
| P3 每层独立 PR 可 revert | PR 切分 | git 历史 |
| P4 保留面行为不变 | — | 删除前后 `harness status --json` diff 为空；smoke 清单 |
| P5 无悬挂引用 | 全 workspace | `cargo test --workspace` + clippy `-D warnings` + `rg 'thread_manager|task_db|review_store|harness-eval'` 零命中（除 CHANGELOG/specs） |
| P6 CHANGELOG breaking 清单 | CHANGELOG.md | 评审核对 16 个方法名 |

## Data Flow

探针：AtomicU64 → 每日 probe_report 事件 → 人读。归档：Postgres → pg_dump
文件（本地 archives/，不进 git——加 .gitignore）。删除不触碰任何运行时数据流。

## Alternatives Considered

- **拆插件仓**：拒绝（用户拍板）——为"未来可能有人要"维护插件仓贵于
  真需要时从 git 历史恢复。
- **rename 表代替 pg_dump**：拒绝——运行中的旧二进制可能仍在写，rename
  会制造运行时错误；dump + 原表不动最稳。
- **一个大 PR 删完**：拒绝——不可局部 revert，review 不可读（U-09）。
- **跳过探针直接删**：拒绝——event_store 只见 hook 类事件，暗流量风险
  真实存在；探针成本一天，误删成本一周。

## Risks

- Security: 无新对外面；删除减少攻击面。
- Compatibility: thread/turn RPC 移除是 breaking——0.x 阶段 + CHANGELOG +
  16 方法清单；已知消费方仅 CLI/dashboard（同仓同 PR 修）。
- Performance: 探针为原子自增，可忽略。
- Maintenance: task 层并入 runtime 的"仅搬运"边界要在 PR 里明示 diff 对照，
  防止顺手重构（U-04/U-07）。

## Test Plan

- [ ] Unit tests: UsageProbe 计数与日报事件
- [ ] Integration tests: 删除后全量 `cargo test --workspace`
- [ ] Manual verification: smoke 清单——serve 启动、`status`、`pr fix --help`、
      webhook 健康、dashboard 首屏；归档恢复演练（restore 到临时库数一致）

## Rollback Plan

每层一个 PR，revert 即回滚该层；数据未动（表原地、dump 在手）。探针 PR
可长期保留（保留面上的计数器对 Phase 2/3 同样有用）。
