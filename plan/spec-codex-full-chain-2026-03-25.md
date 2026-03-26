# Codex 全链路切换 Spec（2026-03-25）

## 目标
将 harness 的默认与内建执行链路统一为 Codex，避免运行时路径对 Claude 的硬编码依赖。

## 现状数据流（简述）
1. 默认 agent 在 `harness-core` 配置层是 `claude`。
2. 未显式指定 agent 时，复杂/关键任务由 `AgentRegistry::dispatch()` 优先选 `claude`。
3. CLI 子命令中 `gc` 与 `pr` 直接实例化 `ClaudeCodeAgent`。
4. `periodic_reviewer` 首阶段固定走“claude + codex 双评审 + 合成”。

## 变更范围
仅修改以下行为，不扩展功能：
1. 默认 agent 从 `claude` 改为 `codex`。
2. 复杂度路由从“优先 claude”改为“优先 codex（若存在），否则回退默认 agent”。
3. `harness gc run` 改为使用 CodexAgent。
4. `harness pr fix/loop` 改为使用 CodexAgent。
5. `periodic_reviewer` 改为单 agent 执行（使用 `review.agent`，未设置时默认 `codex`），移除 claude+codex 双评审硬编码。
6. `harness exec` CLI 的 `--agent` 默认值改为 `codex`。

## 不做事项
1. 不删除 Claude 适配器代码。
2. 不重构 review prompt 语义。
3. 不修改网络/端口/部署行为。

## 影响文件
- `crates/harness-core/src/config/agents.rs`
- `crates/harness-agents/src/registry.rs`
- `crates/harness-cli/src/commands.rs`
- `crates/harness-cli/src/gc.rs`
- `crates/harness-cli/src/cmd/pr.rs`
- `crates/harness-server/src/periodic_reviewer.rs`
- 测试文件（同模块内）与 `crates/harness-core/src/config.rs` 的默认值断言

## Done-When
1. 以上代码编译通过（`cargo check`）。
2. 相关测试通过（最小集：`harness-agents::registry`、`harness-cli::commands`、`harness-core::config`、`harness-server::periodic_reviewer`）。
3. 运行时默认路径不再出现 Claude 硬编码选择（保留可选支持不计入默认链路）。
