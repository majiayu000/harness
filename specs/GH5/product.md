# SpecRail evaluator and rclean pilot validation - Product Spec

GitHub issue: `#5`
Locale: `zh-CN`
Route: `write_spec`

## Summary

GitHub issue `#5` 的目标是把 SpecRail 从文档和模板约定推进到可执行的 evaluator。完成后，维护者和 agent 可以用同一组命令验证一个 SpecRail spec directory 是否具备必要 artifact、任务清单、工作流约束和试点证据，而不是只靠人工阅读。

本 issue 同时引入 `rclean` 作为只读 adoption smoke test。`rclean` 是一个真实 Rust CLI 仓库，适合作为 SpecRail 的第一批外部试点：它有清晰的 CI 命令、已有 `docs/specs/`，但缺少 `AGENTS.md`、issue/PR template 和 SpecRail 标准路径 `specs/GH<number>/product.md` + `tech.md`。试点必须只读，不修改 `rclean` 仓库。

## Problem

1. 当前 SpecRail 约定主要存在于文档和模板中，缺少一个可重复运行的 evaluator 来证明 repo/spec 是否满足最低工作流约束。
2. `check_workflow.py` 需要从基础结构检查扩展到 spec directory artifact 检查，尤其是 `product.md`、`tech.md` 和新的 `tasks.md`。
3. 需要一个顶层 `evaluate.py`，给 agent、CI 和 reviewer 一个稳定入口，并输出可机器读取的结果。
4. 试点需要覆盖真实 repo adoption 的常见风险：spec-first、危险操作 gate、doc-only 直通、CI 命令映射、issue 防重复。
5. 没有数据时不能臆造结果。缺少 artifact、路径、命令或证据时，evaluator 必须返回明确 failure，而不是 silent fallback。

## Goals

- `check_workflow.py` 能验证 SpecRail repo 配置、spec directory 必需 artifact、任务 artifact 和基本内容完整性。
- 新增 `evaluate.py` 作为 evaluator CLI，能组合 workflow/spec/task/smoke 检查并返回稳定 exit code。
- 新增 `tasks_artifact`，标准路径为 `specs/GH5/tasks.md`，用于跟踪 issue `#5` 的可执行任务、验证命令和 done-when。
- 新增 `examples/rclean-smoke.md`，记录 `rclean` 只读试点事实、场景矩阵和通过条件。
- 对 agent 友好：结果必须能被自动解析，稳定 IDs、paths、commands、JSON keys 保持英文。
- 对 reviewer 友好：失败项必须说明缺什么、在哪个 path、建议下一步是什么。

## Non-goals

- 不在本 issue 中修改 `rclean` 仓库、提交 `rclean` issue、创建 `rclean` PR 或改变其 CI。
- 不把 evaluator 变成通用 lint/test runner；它只验证 SpecRail workflow/adoption artifact，不运行目标 repo 的 build/test。
- 不用 evaluator 代替 human approval。涉及安全边界、force push、secret、权限变更和 destructive action 时仍需 human gate。
- 不要求旧仓库立即迁移已有 `docs/specs/`；`rclean` smoke 只记录 gap 和 adoption plan。
- 不在 evaluator 内发起 GitHub 写操作。issue 防重复只要求搜索/报告候选项，不自动创建 issue。

## Users

- `maintainer`: 维护 SpecRail workflow、模板、schema 和 evaluator 的人。
- `agent_worker`: 按 issue/spec 执行任务的 AI worker，需要确定下一步和缺失 artifact。
- `reviewer`: 审查 PR 时需要判断 SpecRail gate 是否真实通过。
- `pilot_repo_owner`: 想在自己的 repo 试用 SpecRail，但不希望试点工具写入代码库。

## Behavior

### `check_workflow.py`

1. 支持从 repo root 运行：

   ```sh
   python3 checks/check_workflow.py --repo . --spec-dir specs/GH5
   ```

2. 当 `--spec-dir specs/GH5` 被传入时，必须验证：
   - `specs/GH5/product.md` exists
   - `specs/GH5/tech.md` exists
   - `specs/GH5/tasks.md` exists
   - artifact 文件非空，并包含对应 issue anchor `#5` 或 `GH5`
   - task IDs 唯一且稳定

3. 缺少 required artifact 时返回 failure。不能因为模板缺失、字段缺失或 repo 类型未知而 pass。

### `evaluate.py`

1. 提供稳定 CLI：

   ```sh
   python3 evaluate.py --repo . --spec-dir specs/GH5 --format json
   ```

2. `--format json` 输出必须包含这些 JSON keys：
   - `status`
   - `repo`
   - `spec_dir`
   - `checks`
   - `artifacts`
   - `errors`
   - `warnings`
   - `next_actions`

3. `status` 只能是：
   - `pass`
   - `fail`
   - `needs_human`

4. exit code 语义：
   - `0`: `status=pass` 或 `status=needs_human`，表示 artifact 检查没有失败，但可能仍需人工 gate
   - `1`: deterministic check failed，表示 artifact 或证据失败
   - `2`: CLI usage/config error

5. evaluator 不应修改 repo。所有检查默认只读。

### `tasks_artifact`

1. 每个 SpecRail issue spec directory 可以包含 `tasks.md`。
2. 对 issue `#5`，`specs/GH5/tasks.md` 是 required artifact。
3. 每个 task 必须有 stable ID、owner scope、done-when 和 verification command 或 review proof。
4. task status 使用 Markdown checkbox，便于人工和简单 parser 同时读取。

### `rclean` smoke

`examples/rclean-smoke.md` 必须覆盖以下 smoke scenario：

- `rclean.new_rule_spec_first`: 新增规则前必须先有 issue/spec path，不允许直接改 Rust rule code。
- `rclean.security_boundary_gate`: 删除文件、路径遍历、权限扩大、secret 相关变更必须 route 到 human gate。
- `rclean.doc_only_direct`: 小型 README/docs 改动可走 direct/doc-only，但仍要说明验证方式。
- `rclean.ci_command_mapping`: Rust CI 命令必须映射为 adoption evidence。
- `rclean.issue_dedupe`: 发现 `drafts/rclean-issues-draft-2026-05-25.md` 中 `NOT SUBMITTED YET` 草稿时，不创建重复 issue。

## Acceptance Criteria

- `specs/GH5/product.md`、`specs/GH5/tech.md`、`specs/GH5/tasks.md` 存在并互相引用。
- `examples/rclean-smoke.md` 存在，明确标注 `rclean` 试点为 read-only。
- `evaluate.py` 的 JSON 输出稳定、可解析，失败时指向具体 missing/invalid path。
- `check_workflow.py` 对缺失 `tasks.md`、重复 task ID、空 `product.md` 或空 `tech.md` 返回 failure。
- evaluator 不执行 destructive command，不写入被评估 repo，不自动提交 issue/PR。
- `rclean` smoke 可以在不修改 `/Users/lifcc/Desktop/code/AI/tool/rclean` 的情况下完成 adoption 评估。

## Done When

- 本 spec directory 的 required artifacts 齐全。
- 本 issue 的 evaluator implementation PR 能运行：

  ```sh
  python3 checks/check_workflow.py --repo . --spec-dir specs/GH5
  python3 evaluate.py --repo . --spec-dir specs/GH5 --format json
  ```

- Reviewer 可以从 `evaluate.py` 输出中看到 issue `#5`、`tasks_artifact`、`rclean_smoke` 的 pass/fail 状态。
