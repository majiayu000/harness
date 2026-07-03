# Product Spec

## Linked Issue

GH-1434

## 用户问题

内核化收缩 Phase 1（架构分析与决策见 better/reports/harness-kernel-design.md，
2026-07-03 拍板：直接删、不拆插件仓）。生产数据显示体量与价值严重倒挂：

- workflow runtime：216,161 runtime_events / 67,436 jobs / 1,877 workflows——全部真实流量
- thread/turn 生命周期层（16 个 JSON-RPC 方法 + thread_manager/thread_db）：历史共 **9 个 thread**
- task 层（task_db/task_executor/task_runner）：历史共 **62 个 task**
- harness-eval（848 LOC）：**0** 次 eval_runs
- review_store：**0** 条 review_findings（评审实际走 GitHub bot）

三代状态机叠加，第三代（workflow runtime）已事实性取代前两代。死代码不仅是
维护成本：它撑大了对外面（30 个 RPC 方法）、拖慢构建、并让新贡献者无法分辨
哪条路径是活的。

## 目标

1. 删除零流量层：thread/turn RPC 面、task 层、harness-eval、review_store。
2. 删除前有安全探针：每个待删面先落使用计数器，7 天零计数（或维护者显式
   豁免并留档）后方可删除。
3. 数据先归档后删码：9 threads + 62 tasks 及关联表 pg_dump 存档；本阶段
   只归档不 drop 任何表。
4. workflow runtime 主路径行为完全不变。

## 非目标

- Phase 2（GitHub 事实核验替代 activity envelope）、Phase 3（智能层删除：
  rules 非 execpolicy 部分/skills/GC/learn/self_evolution/q_value/
  complexity_router）——各自另立 issue。
- 不 drop 任何 Postgres 表或 schema。
- 不改 workflow runtime 派发语义；不与 #1430 熔断工作冲突。
- 不重构保留模块的代码风格（U-07）。

## Behavior Invariants

1. 删除动作仅在探针证据（7 天零计数）或书面豁免存在后发生；removal PR
   必须引用计数结果。
2. 归档先于删除：threads/tasks 相关表的 pg_dump 产物存在且可恢复
   （文档记录恢复命令）后，代码删除才可合并。
3. 每层删除是独立 PR（thread/turn、task、eval、review_store 至少四个），
   任一可单独 revert（U-09）。
4. 保留面的行为不变：`harness serve/status/pr fix/execpolicy/reconcile`、
   webhook intake、dashboard 的 workflow 视图，删除前后输出一致。
5. 删除后 workspace 无悬挂引用：protocol 定义、router 注册、CLI 子命令、
   dashboard 面板同步清理；`cargo test --workspace` 与 clippy `-D warnings` 绿。
6. 对外面收缩可见：RPC 方法数从 ~30 降到 ~14（thread×8 + turn×8 移除），
   在 CHANGELOG 记录 breaking change 清单。

## 验收标准

- [ ] 探针 PR 先行合并，计数器对 thread/*、turn/*、task 执行路径、eval、
      review_store 生效并可在日志/事件中查询。
- [ ] 归档产物：threads(9)、tasks(62) 及关联表的 dump 文件 + 恢复文档。
- [ ] 四个删除 PR 合并后：harness-eval crate 不在 workspace；review_store、
      thread_manager/thread_db、task_db/task_executor/task_runner 模块不存在；
      protocol 中 thread/turn 方法定义移除。
- [ ] 净删除 LOC > 10k，在最终 PR 汇总。
- [ ] `cargo test --workspace` 退出码 0；smoke：serve 启动 + status +
      `pr fix --help` + dashboard 首屏加载。

## 边界情况

- **暗流量**：event_store 只记 hook 类事件，可能存在不经它的调用路径——
  这正是探针存在的原因；探针必须覆盖 RPC 入口和模块公开函数两层。
- **CLI 兼容**：若 CLI 存在 thread 相关子命令，删除属 breaking change，
  版本号按 SemVer MINOR（0.x 阶段）+ CHANGELOG 记录。
- **dashboard 依赖**：若面板查询 thread/task 端点，先改面板后删端点，
  避免中间态白屏。
- **进行中分支**：feat/usage-monitor-dashboard 改的 usage_monitor 属保留面，
  无冲突；若其触碰 task_db，删除 PR 需先 rebase 协调。
- **workflow runtime 对 task 层的残余依赖**：task 层"合并进 runtime 或删除"
  以依赖扫描结果为准——有真实依赖的部分并入，无依赖的删除。
