# Open PR 分析（2026-03-05）

数据快照时间：`2026-03-05 12:50 CST`
仓库：`majiayu000/harness`

## 1. 现状

- 当前 open PR 数量：`14`
- `mergeable_state=clean`：`8` 个（可合并，但未闭环）
- `mergeable_state=dirty`：`6` 个（与 main 冲突）

## 2. 为什么很多 PR 还开着

## 原因 A：外部 review 服务配额阻塞（不是代码本身卡住）

- PR `#51`, `#52`
- 证据：PR 评论里多次出现 `gemini-code-assist` 的
  `You have reached your daily quota limit. Please wait up to 24 hours...`
- 影响：harness 的 fresh-review 门禁无法满足，任务在 `waiting` 或因等待上限失败。

## 原因 B：PR 冲突（dirty），无法直接 merge

- PR `#30`, `#34`, `#35`, `#45`, `#46`, `#48`
- 这些 PR 都是早期分支，主干已继续演进，导致冲突。
- 典型后果：即便 review 可接受，也需要 rebase/cherry-pick 后才能合并。

## 原因 C：同一 issue 多次重跑，产生“并行候选 PR”但未清理

- issue `#29` 相关：`#30`, `#44`, `#47`, `#50`, `#52`
- issue `#41` 相关：`#42`, `#45`, `#51`
- issue `#16` 相关：`#46`, `#48`（而 `#49` 已在 `2026-03-05` 合并）
- 影响：旧 PR 仍 open，视觉上像“很多没做完”，本质是“重试分支未关闭”。

## 原因 D：历史流程“done 不等于 merged”留下存量

- 早期流程只到 `LGTM/done`，不会自动 merge，所以留下一批 open PR。
- 本次已补齐：`LGTM -> gh pr merge --squash --delete-branch -> done`。

## 原因 E：review 有 comment 但没有后续修复提交

- 多个 clean PR（如 `#33`, `#42`, `#43`, `#44`, `#47`, `#50`）最新 review 都是 `COMMENTED`。
- 在旧流程下，容易出现“有评论但没人继续处理并合并”的停滞。

## 3. 按 PR 快速结论

| PR | mergeable_state | review 状态 | 主要未关闭原因 |
|---|---|---|---|
| 52 | clean | 无 review（且有 quota 警告） | 外部 review 配额阻塞 + #29 候选之一 |
| 51 | clean | COMMENTED（有 quota 警告） | fresh-review 未稳定回流 + #41 候选之一 |
| 50 | clean | COMMENTED | #29 旧候选，未继续修复/合并 |
| 48 | dirty | COMMENTED | 与 main 冲突，且 #16 已有更新候选 |
| 47 | clean | COMMENTED | #29 旧候选，未收敛 |
| 46 | dirty | COMMENTED | 与 main 冲突，且 #16 已有更新候选 |
| 45 | dirty | COMMENTED | 与 main 冲突，且 #41 已有更新候选 |
| 44 | clean | COMMENTED | #29 旧候选，未收敛 |
| 43 | clean | COMMENTED | 可合并但历史遗留未处理 |
| 42 | clean | COMMENTED | 可合并但历史遗留未处理 |
| 35 | dirty | COMMENTED | 大体量分支 + 冲突 + review 风险项 |
| 34 | dirty | COMMENTED | 大体量分支 + 冲突 + review 风险项 |
| 33 | clean | COMMENTED | 可合并但历史遗留未处理 |
| 30 | dirty | COMMENTED | #29 最早候选，已过时且冲突 |

## 4. 结论

这些 PR 长期开着，不是单一原因：

1. 外部 reviewer 配额问题（尤其 `#51/#52`）
2. 旧分支与 main 冲突（dirty）
3. 同一 issue 重跑产生多个候选 PR，但没有“自动关闭被替代 PR”的机制
4. 历史上 done 未强制 merge，形成存量

简化成一句话：
**当前 open PR 多，主要是“历史存量 + 候选重复 + 外部 review 配额阻塞”，而不是单纯开发没做。**
