# Product Spec

## Linked Issue

GH-1430

## 用户问题

失败风暴复盘（66,971 runtime jobs，failed=37,694，2026-07-03 快照；数据底稿
better/reports/harness-failure-taxonomy.md）：68% 的失败集中在 3 个风暴日
（06-06 / 06-09 / 06-10，当日失败率 96–99%）；单一错误类
`codex exited with exit status: 1: stderr=[Reading additional input` 独占
20,170 条（53.5%），其中 95% 落在 4 个风暴日，07-02 又复发 320 条。

根因模式：agent CLI 在配额耗尽或等待交互输入时立即 exit 1，而派发循环没有
跨 tick 的失败记忆，把整个 backlog 持续派进同一个已死的 runtime——06-09 当天
7,806 失败 vs 62 成功，token 照烧、产出接近零。

## 目标

1. 失败在源头分类：agent 退出类失败带稳定的 `failure_class` 落库，不再只有
   一个不透明的 `agent execution failed` 字符串。
2. 派发路径熔断：同一 runtime profile 连续 N 个同类失败 → 暂停该 profile 的
   派发一个冷却期，发 error 级事件，`harness status` 可见熔断状态。
3. 冷却后半开探测：探测成功关闸恢复，失败则带退避重新打开。

## 非目标

- 不重设计现有重试语义。
- envelope/结构化输出强制化（另开 issue）。
- WorktreeCollision 调度修复（另开 issue）。
- 跨 profile / 跨 repo 的联动熔断（v1 只做 per-profile）。

## Behavior Invariants

1. 熔断只暂停"派发新 job"，不杀正在运行的 job，不改已落库 job 的状态。
2. 熔断触发必须是 error 级事件（U-29：造成用户可见产出缺失的降级不允许
   warning+继续）；事件里带 profile、触发类、连续失败数、冷却截止时间。
3. 熔断器状态机：closed → (N 个连续同类失败) → open → (冷却到期) →
   half-open → 探测成功 → closed；探测失败 → open（冷却时间×退避系数）。
4. 计数按 (runtime_profile, failure_class) 维度；任一成功 job 将该 profile
   的连续计数清零。
5. 手动恢复入口存在（复位指定 profile 的熔断器），且复位动作本身留事件。
6. 分类失败（无法识别的错误串）归入 `unclassified`，unclassified 也参与熔断
   计数——不能因为"不认识这个错误"就放它刷穿队列。
7. 默认阈值与冷却时间可配置且有文档；关闭熔断必须是显式配置项，默认开启。

## 验收标准

- [ ] runtime_jobs 上可见 failure_class；`harness status --json` 暴露熔断状态。
- [ ] 默认阈值：同 profile 连续 5 个同类失败触发；冷却默认 10 分钟，退避×2，
      上限 2 小时（数值允许 spec 评审时校准）。
- [ ] 单测覆盖：触发、冷却、半开恢复、半开再失败退避、成功清零计数。
- [ ] 风暴回放测试：以 06-09 的失败序列形状（同类失败连续到达）作 seed，
      断言第 5 个失败后停止派发——历史上 7,806 次派发在此机制下应缩为个位数
      加每冷却期一次探测。
- [ ] config/default.toml.example 记录全部新配置键。
- [ ] `cargo test --workspace` 绿（需要 DB 的测试按仓库现有惯例跑）。

## 边界情况

- 服务重启：熔断器状态可以丢（内存态可接受，v1 不要求持久化），但重启后
  风暴若仍在，会在 N 个失败内重新触发——可接受。
- 多 worker 并发 tick：计数器需要并发安全；v1 允许最坏情况下多派发少量 job
  （计数近似即可），不追求严格一致。
- 失败类交错（A,B,A,B…）：按类分别计数，都到不了 N 则不触发——这是设计内
  行为；若真实风暴呈交错形态，靠"同 profile 总失败率"扩展再议（非目标）。
- half-open 探测 job 的选取：取队首下一个自然 job，不构造合成 job。
