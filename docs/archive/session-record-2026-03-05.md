# Harness 会话执行记录（2026-03-05）

## 1. 目标与范围

本次工作围绕你提出的流程要求执行：

1. 串行提交并执行任务（不是并发）
2. 任务不能只停在“产出 PR”，要按 review/fix 流程闭环
3. 识别“done 但不可靠”的根因并做工程化改进
4. 验证 codex 是否真正在 harness 内执行
5. 把问题与修复完整落文档

---

## 2. 执行方式（怎么做的）

### 2.1 串行执行策略

采用固定模式：

1. `POST /tasks` 提交一个任务
2. `GET /tasks/{id}` 轮询直到 `done/failed`
3. 记录 `task_id/status/turn/pr_url/error/rounds`
4. 上一个结束后才提交下一个

示例（本次实际使用）：

```bash
curl -X POST http://localhost:9800/tasks -H 'Content-Type: application/json' -d '{"issue":41,...}'
curl http://localhost:9800/tasks/<task_id>
```

### 2.2 流程可靠性校验策略

在服务端增加/验证硬约束，不依赖模型文本自觉：

1. 缺 `PR_URL` 且任务应产出 PR 时，直接失败
2. review 环节读取 GitHub 快照（author/commits/reviews）做门禁
3. 修复后必须拿到 fresh re-review 才允许继续
4. 有 actionable external feedback 时，不允许直接 LGTM 过关
5. `LGTM` 后执行 `gh pr merge --squash --delete-branch`，再标记 done

---

## 3. 任务执行记录（含多轮重跑）

## 第一轮（初始串行）

1. `#41`
- task: `57f46397-015e-4143-b09f-0f8265a1152b`
- 结果: `failed`
- 错误: `Task expected PR output, but agent did not emit PR_URL=<url>`
- 结论: “无 PR 不能 done”硬约束生效

2. `#29`
- task: `8555e236-6989-48cd-a05f-76101abb4b5b`
- 结果: `failed`
- 错误: `Turn 1 timed out after 1200s`

3. `#16`
- task: `738221c5-e273-4fd4-9105-4f8c04116e89`
- 结果: `done`
- PR: `https://github.com/majiayu000/harness/pull/49`
- rounds: `pr_created -> waiting -> fixed -> waiting -> lgtm`

## 第二轮（codex 路径验证 + 重跑）

4. `#41`（codex 重跑）
- task: `21b6f61b-d7ff-4171-b683-2047eae4bb82`
- PR: `https://github.com/majiayu000/harness/pull/51`
- 结果: `failed`
- 错误: `PR #51 did not receive a fresh external review after latest fix commit`
- rounds: 出现多次 `fixed` 与 `waiting`，最终被 fresh-review 门禁拦截

5. `PR #51` 续跑
- task: `e74306c1-f9b2-4197-8693-8e1041cb322a`
- 结果: `failed`
- 错误: `Cannot accept LGTM for PR #51: actionable external feedback exists but no FIXED round was completed`
- 结论: “有 actionable feedback 不可直接 LGTM”门禁生效

6. `#29`（codex 重跑）
- task: `e4b39d3c-c4c8-4f6f-af73-4f7069249323`
- PR: `https://github.com/majiayu000/harness/pull/52`
- 结果: `failed`
- 错误: `PR #52 has no external review after 3 checks`

## 第三轮（放宽 waiting 重试预算后）

7. `PR #52` 续跑（新参数）
- task: `c6455b12-302c-4779-8193-27392f8ff5cb`
- 参数: `wait_secs=60, max_waiting_retries=12`
- 状态（记录时）: `implementing`（进行中）

---

## 4. PR 状态快照（记录时）

1. PR #49
- URL: `https://github.com/majiayu000/harness/pull/49`
- 状态: `MERGED`
- mergedAt: `2026-03-05T03:28:44Z`

2. PR #51
- URL: `https://github.com/majiayu000/harness/pull/51`
- 状态: `OPEN`
- 最新 review: `COMMENTED`

3. PR #52
- URL: `https://github.com/majiayu000/harness/pull/52`
- 状态: `OPEN`
- 最新 review: 暂无外部 review

---

## 5. 代码改动清单（做了什么）

### 5.1 Codex 接入与默认切换

1. codex 执行参数修正（可写、非交互自动）
- 文件: `crates/harness-agents/src/codex.rs`
- 关键位置: 使用
  `codex exec --skip-git-repo-check --dangerously-bypass-approvals-and-sandbox`

2. server 启动时注册 codex agent
- 文件: `crates/harness-cli/src/commands.rs`
- 关键位置: `agent_registry.register("codex", ...)`

3. 默认 agent 切换为 codex
- 文件: `config/default.toml`
- 关键位置: `default_agent = "codex"`

### 5.2 流程硬约束（task_executor）

- 文件: `crates/harness-server/src/task_executor.rs`

1. 缺 PR_URL 直接失败（应产出 PR 的任务）
2. 读取 GitHub PR 快照做 review 门禁
3. 修复后要求 fresh re-review
4. 有 actionable feedback 且未 FIXED，不允许 LGTM
5. timeout/error 写入 rounds，提升可观测性
6. `LGTM` 后执行 merge，merge 成功才 done

### 5.3 等待重试预算可配置

1. 请求参数新增 `max_waiting_retries`（默认 6）
- 文件: `crates/harness-server/src/task_runner.rs`

2. executor 使用请求中的重试预算
- 文件: `crates/harness-server/src/task_executor.rs`

3. GC adopt 任务补齐该参数
- 文件: `crates/harness-server/src/handlers/gc.rs`

### 5.4 文档化

已将问题与修复追加至：
- `docs/issues-encountered.md`
- 新增条目包含 Issue 14~20（含症状、根因、修复、经验）

---

## 6. 验证与回归

本次改动后已执行：

1. `cargo test -q -p harness-server -p harness-cli`
2. `cargo test -q`

结果：全部通过。

---

## 7. 当前结论

1. 不是“完全没成功”
- 至少有 1 条完整闭环成功：PR #49 已 merged

2. 失败主要来自两类真实门禁触发
- 外部 review 未回流/未 fresh 回流
- agent 试图在有 actionable feedback 时直接 LGTM

3. 系统行为已从“假成功”转为“可解释失败”
- 每个失败都有明确 error 与 rounds 轨迹

4. 仍在优化中的点
- 外部 review 时延/不稳定导致等待失败，需要更长预算或外部 review SLA 改善

---

## 8. 记录时服务状态

- server: `http://localhost:9800` 正常
- 当前活跃任务：`c6455b12-302c-4779-8193-27392f8ff5cb`（PR #52 续跑）

