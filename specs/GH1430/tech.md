# Tech Spec

## Linked Issue

GH-1430

## Product Spec

`specs/GH1430/product.md`

## Codebase Context

| Area | Files | Current behavior | Why relevant |
| --- | --- | --- | --- |
| 派发 tick | `crates/harness-server/src/workflow_runtime_worker.rs` | `run_runtime_job_worker_tick` 每 tick 取 job 执行；`WorkerTickStats` 只统计当前 tick，无跨 tick 记忆 | 熔断检查点与计数更新点 |
| job 执行 | `crates/harness-server/src/workflow_runtime_worker/executor.rs` | 运行 agent、落 job 状态 | 失败分类的采集点（拿到原始错误串的地方） |
| 错误类型 | `crates/harness-core/src/error.rs:38` | `agent execution failed: {0}` 单一包装 | 需要附带 `failure_class` |
| job 存储 | `workflow_runtime.runtime_jobs`（Postgres），`data` jsonb 已有 `error` 字段 | status + 不透明 error 串 | `failure_class` 写入 `data`，不加列、不迁移 schema |
| 状态展示 | `harness status`（CLI）+ `/api/workflows/runtime/*` | 已聚合 job 状态计数 | 暴露熔断器状态 |
| 配置 | `crates/harness-core/src/config/`、`config/default.toml.example` | workflow 配置已有先例（本地 feat 分支正在改 `config/workflow.rs`） | 新增 breaker 配置节 |
| 事件 | `harness-observe`（events/OTLP） | 已有事件管线 | error 级熔断事件从这里发 |

## Proposed Design

新模块 `crates/harness-server/src/workflow_runtime_worker/circuit_breaker.rs`：

```rust
pub struct BreakerRegistry {                 // 服务级单例，Mutex<HashMap>
    per_profile: HashMap<ProfileKey, BreakerState>,
    config: BreakerConfig,
}
enum BreakerState {
    Closed { consecutive: HashMap<FailureClass, u32> },
    Open   { class: FailureClass, until: Instant, backoff_exp: u32 },
    HalfOpen { class: FailureClass, backoff_exp: u32 },
}
```

**失败分类（executor 侧）**：纯函数 `classify_agent_failure(&err_str) -> FailureClass`，
regex 表映射（顺序匹配，首中即止）：

| class | 匹配 |
| --- | --- |
| `quota-interactive-wait` | `Reading additional input`、`hit your (usage )?limit` |
| `cli-missing-file` | `No such file or directory` |
| `worktree-collision` | `WorktreeCollision` |
| `structured-output-missing` | `no harness-activity-result` |
| `sandbox-permission` | `sandbox|permission denied` |
| `unclassified` | 其余 |

class 写入 job `data.failure_class`（jsonb，无 schema 迁移）。

**熔断检查（worker tick 侧）**：
- 派发前：`registry.allow(profile)` — Open 且未到期 → 跳过该 profile 的 job
  （保留在队列，`not_before` 顺延冷却期，避免空转热循环）；Open 到期 →
  转 HalfOpen，放行恰好 1 个 job。
- 落结果后：成功 → `record_success(profile)`（Closed 清零计数；HalfOpen →
  Closed + 关闸事件）；失败 → `record_failure(profile, class)`（Closed 计数
  +1，达阈值 → Open + error 事件；HalfOpen → Open，`backoff_exp+1`）。

**配置**（`[workflow.circuit_breaker]`）：
`enabled = true`、`consecutive_failures = 5`、`cooldown_secs = 600`、
`backoff_factor = 2.0`、`max_cooldown_secs = 7200`。

**状态暴露**：status 聚合响应加 `circuit_breakers: [{profile, state, class,
consecutive, cooldown_until}]`；CLI text 输出加一行（仅在有非 Closed 时显示）。

**并发**：registry 用 `parking_lot::Mutex`（或 tokio Mutex，按仓库惯例）；
计数近似一致即可（product 边界情况 2）。内存态，不持久化（边界情况 1）。

## Product-to-Test Mapping

| Product invariant | Implementation area | Verification |
| --- | --- | --- |
| P1 只停派发不杀运行中 job | worker tick 的 allow 检查位置 | 单测：open 状态下 in-flight job 正常落库 |
| P2 error 级事件 | observe 事件发射 | 单测断言事件级别与字段 |
| P3 状态机 | circuit_breaker.rs | 状态机单测（触发/冷却/半开/退避） |
| P4 按 (profile, class) 计数、成功清零 | record_* | 单测：交错失败不触发；成功清零 |
| P5 手动复位 + 事件 | JSON-RPC method 或 CLI 子命令 `harness runtime breaker reset <profile>` | 单测 + 手动验证 |
| P6 unclassified 参与计数 | classify + record | 单测：未知错误串连续 5 个也触发 |
| P7 默认开启、可配置 | config | 配置解析单测 |
| 风暴回放 | 集成测试 | seed 06-09 序列形状，断言第 5 个失败后停止派发 |

## Data Flow

executor 失败 → classify → job.data.failure_class 落库 → registry.record_failure
→ (达阈) Open + error 事件 → 下个 tick allow() 拦截该 profile → 冷却到期
half-open 放行 1 个 → 结果决定 Closed/re-Open。无新外部依赖、无 schema 迁移。

## Alternatives Considered

- **持久化熔断状态到 DB**：拒绝（v1）——重启后风暴仍在会在 N 个失败内重新
  触发，内存态足够；持久化增加迁移与一致性成本。
- **全局（跨 profile）熔断**：拒绝——一个 profile 的配额风暴不应停掉其他
  runtime 的正常工作。
- **在 retry 层做（改重试策略）**：拒绝——风暴不是"该不该重试单个 job"的
  问题，是"该不该继续向死 runtime 派发新 job"的问题，层级不同。
- **失败率滑动窗口代替连续计数**：v1 用连续计数（更简单、可解释）；窗口版
  作为后续演进，接口上 `record_*` 已可替换实现。

## Risks

- Security: 无新外部面；复位命令走既有 JSON-RPC 鉴权路径。
- Compatibility: status JSON 加字段为 additive；jobs `data` 加 key 无迁移。
- Performance: 每次派发一次 HashMap 查找 + Mutex，可忽略。
- Maintenance: regex 分类表与真实错误串会漂移——每类必须带取自生产数据的
  fixture 串做单测锚定；unclassified 兜底保证漂移不产生安全洞。

## Test Plan

- [ ] Unit tests: classify 表（每类正例 + unclassified）、状态机全转移、
      并发 record 冒烟
- [ ] Integration tests: 风暴回放（同类连续失败序列 → 第 5 个后派发停止）
- [ ] Manual verification: 本地 serve，用假 profile 连发失败 job，观察
      status 输出与事件

## Rollback Plan

`[workflow.circuit_breaker] enabled = false` 即完全旁路；代码回滚为 revert
单 PR，无 schema 变更需要清理。
