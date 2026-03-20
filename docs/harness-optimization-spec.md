# Harness 优化 SPEC — 基于 Knowledge Scout 发现

> 2026-03-19 | 来源：Fowler SDD 批判、TDAD 论文、Faithfulness Loss 论文、Spec-Kit Constitution
> 状态：DRAFT — 待用户确认后执行

## 目标

关闭 Harness 的 Declaration-Execution Gap，从"框架声明 100% / 运行时执行 ~50%"提升到可验证的端到端闭环。

**不做的事**：不新增子系统，不扩展协议方法，不添加新 crate。

## 约束

- 每次修改后 `cargo check --workspace`
- 提交前 `RUSTFLAGS="-Dwarnings" cargo check --workspace --all-targets`
- 遵守 CLAUDE.md 中的 VibeGuard 豁免规则
- 不修改 `TurnInterceptor` trait 签名（U-01）

---

## Phase 1: 闭合拦截循环（Interceptor Loop）

**问题**: `hook_enforcer.rs` 实现了 `TurnInterceptor`，但 `AppState.interceptors` 始终为 `vec![]`。

### 1.1 注册 HookEnforcer 到 AppState

**文件**: `crates/harness-server/src/http.rs`
**位置**: `build_app_state()` 函数末尾（约 L350）

```rust
// 当前：interceptors: vec![]
// 改为：
let hook_enforcer = Arc::new(HookEnforcer::new(
    Arc::clone(&engine_svc.rules),
    Arc::clone(&observe_svc.events),
    true, // enabled
));
// ...
interceptors: vec![hook_enforcer],
```

**验证**: `cargo test -p harness-server` 通过 + 新增集成测试验证 interceptor 被调用。

### 1.2 修复 HookEnforcer 的 SessionId 泄漏

**文件**: `crates/harness-server/src/hook_enforcer.rs:120`
**问题**: `SessionId::new()` 创建匿名 ID，事件无法关联到实际会话。

```rust
// 当前：Event::new(SessionId::new(), ...)
// 改为：在 HookEnforcer 中持有 session_id 或从 ToolUseEvent 传递
```

**方案**: 给 `ToolUseEvent` 添加 `session_id: Option<SessionId>` 字段（不破坏现有接口，Option 默认 None）。

### 1.3 集成测试

**新文件**: `crates/harness-server/tests/interceptor_enforcement.rs`

```
测试场景:
1. turn/start → post_tool_use 被调用 → 违规反馈写入 EventStore
2. turn/start → pre_execute → InterceptResult::block → Turn 被拒绝
3. 无违规时 → 正常通过
```

---

## Phase 2: Golden Principles 注入（Constitution 模式）

**来源**: Spec-Kit 的 Constitution 概念 + Fowler 的 SDD 分析
**问题**: Golden Principles 定义在 `prompts.rs` 中但仅作为文档，不自动注入 agent 上下文。

### 2.1 Constitution 文件

**新文件**: `config/constitution.md`

```markdown
# Harness Constitution — 不可变架构原则

## GP-01: 可执行制品
所有操作必须产生可审查的制品（PR、草案、日志），不允许仅修改内存状态。

## GP-02: 诊断优先
先诊断后修复。GC 必须先生成信号报告，再生成修复草案。

## GP-03: 机械执行
规则检查必须是确定性的。相同输入 → 相同输出。不依赖 LLM 判断合规性。

## GP-04: 可观测操作
每个 Turn 的输入/输出/工具调用都记录到 EventStore。

## GP-05: 保持地图
配置（config）和运行时状态（runtime）必须一致。单一配置源。
```

### 2.2 Turn 启动时注入

**文件**: `crates/harness-server/src/handlers/turn.rs`
**位置**: `handle_turn_start()` 中构建 agent prompt 前

```rust
// 读取 constitution 并作为系统上下文前缀注入
let constitution = include_str!("../../../config/constitution.md");
let full_prompt = format!("{}\n\n---\n\n{}", constitution, user_prompt);
```

**验证**: Turn 的 agent 执行日志中包含 GP-01~GP-05 内容。

---

## Phase 3: GC 增量扫描（影响范围分析）

**来源**: TDAD 论文 — 依赖图 + 影响范围分析，回归率 -70%
**问题**: GC 每次全量读取 EventStore，`checkpoint_path` 声明但未接线。

### 3.1 接线 checkpoint_path

**文件**: `crates/harness-server/src/http.rs` — `build_app_state()`
**问题**: `GcAgent::new()` 后未调用 `.with_checkpoint()`（U-26 鸿沟）

```rust
// 当前：
let gc_agent = Arc::new(GcAgent::new(config.gc.clone(), signal_detector, draft_store, project_root.clone()));

// 改为：
let checkpoint_path = config.server.data_dir.join("gc-checkpoint.json");
let gc_agent = Arc::new(
    GcAgent::new(config.gc.clone(), signal_detector, draft_store, project_root.clone())
        .with_checkpoint(checkpoint_path)
);
```

### 3.2 GC 工具白名单配置化

**文件**: `crates/harness-gc/src/gc_agent.rs:100`
**问题**: `allowed_tools` 硬编码为 `["Read", "Grep", "Glob"]`

```rust
// 当前：allowed_tools: vec!["Read".into(), "Grep".into(), "Glob".into()]
// 改为：从 GcConfig 读取
```

**文件**: `crates/harness-core/src/config/gc.rs`
```rust
// 新增字段：
pub allowed_tools: Option<Vec<String>>, // 默认 ["Read", "Grep", "Glob"]
```

### 3.3 变更影响范围分析（简化版 TDAD）

**文件**: `crates/harness-gc/src/gc_agent.rs` — `run()` 方法

在 GC 运行前，通过 `git diff --name-only` 获取变更文件列表，只对变更文件及其 import 依赖做信号检测：

```rust
// 新增辅助函数
async fn changed_files_since_checkpoint(&self) -> Vec<PathBuf> {
    // 读取 checkpoint 的 commit hash
    // git diff --name-only <checkpoint_hash>..HEAD
    // 返回变更文件列表
}
```

**不做**: 完整的 AST 依赖图（过度设计）。用 `git diff` + `grep import` 做简化版即可。

---

## Phase 4: 配置一致性修复

**来源**: U-11 规则 + gap-analysis 审计

### 4.1 统一 ProjectId 来源

**文件**: `crates/harness-server/src/http.rs:300-303`
**问题**: `SignalDetector::new` 使用 `ProjectId::new()` 创建匿名 ID

```rust
// 当前：ProjectId::new()
// 改为：从 project_registry 或 config 获取真实 ID
```

### 4.2 硬编码常量配置化

**文件**: `crates/harness-server/src/http.rs:27-28`

| 常量 | 当前值 | 移至 |
|------|--------|------|
| `MAX_WEBHOOK_BODY_BYTES` | `512 * 1024` | `ServerConfig.max_webhook_body_bytes` |
| `SIGNAL_RATE_LIMIT_PER_MINUTE` | `100` | `ObserveConfig.signal_rate_limit` |

### 4.3 过时文档清理

**文件**: `docs/gap-analysis.md`
**问题**: Dimension Details (L25-166) 与 Summary (L8) 数据矛盾（55% vs 95%）

同步更新过时段落，标注已修复项。

---

## Phase 顺序与依赖

```
Phase 1 (闭合拦截循环) ← 无依赖，立即可做
  ↓
Phase 2 (GP 注入) ← 依赖 Phase 1 的 interceptor 基础
  ↓
Phase 3 (GC 增量) ← 独立于 1/2，可并行
  ↓
Phase 4 (配置一致) ← 独立，可并行
```

**建议执行顺序**: Phase 1 → Phase 4（快速修复） → Phase 2 → Phase 3

## 验收标准

| Phase | Done-when |
|-------|-----------|
| 1 | `cargo test -p harness-server` 通过 + interceptor_enforcement 测试绿 |
| 2 | Turn 执行日志中包含 GP-01~05 + constitution.md 存在 |
| 3 | GC 运行日志显示"增量扫描 N 个文件"而非全量 + checkpoint 文件更新 |
| 4 | `grep -r "ProjectId::new()" crates/harness-server/src/http.rs` 返回 0 + 无硬编码常量 |

## 文件影响清单

| 文件 | 修改类型 | Phase |
|------|----------|-------|
| `crates/harness-server/src/http.rs` | 修改 build_app_state | 1, 3, 4 |
| `crates/harness-server/src/hook_enforcer.rs` | 修复 SessionId | 1 |
| `crates/harness-core/src/interceptor.rs` | 添加 session_id 到 ToolUseEvent | 1 |
| `crates/harness-server/tests/interceptor_enforcement.rs` | 新建 | 1 |
| `config/constitution.md` | 新建 | 2 |
| `crates/harness-server/src/handlers/turn.rs` | GP 注入 | 2 |
| `crates/harness-gc/src/gc_agent.rs` | checkpoint 接线 + 影响分析 | 3 |
| `crates/harness-core/src/config/gc.rs` | 添加 allowed_tools | 3 |
| `docs/gap-analysis.md` | 同步更新 | 4 |

**总计**: 8 个文件修改 + 2 个新文件 = 10 个文件
