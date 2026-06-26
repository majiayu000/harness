# Adoption matrix and fixture validation - Product Spec

GitHub issue: `#13`
Locale: `zh-CN`
Route: `write_spec`

## Summary

GitHub issue `#13` 的目标是把已经跑过的真实仓库 pilot 固化为
SpecRail 自己可展示、可检查的 adoption matrix。完成后，维护者和
agent 不需要从聊天记录推断哪些仓库验证过 SpecRail，而是可以从仓库内
的文档和 fixture 看到证据。

## Problem

1. `rclean`、`litellm-rs`、`Claude-Code-Monitor` 已经分别提供了 read-only
   smoke、PR gate、issue/spec/PR flow 信号，但证据分散在 specs、tests、
   examples 和外部 GitHub 状态里。
2. 目前 `evaluate.py` 只检查 `rclean` smoke，不能说明 SpecRail 是否已经有
   多仓库 adoption 记录。
3. 没有 machine-readable adoption fixture 时，matrix 容易变成 README 文案，
   后续改动可能删除 pilot 证据而测试不失败。

## Goals

- 新增 `docs/ADOPTION_MATRIX.md`，记录 adoption levels、当前 pilot 仓库、
  证据路径、状态和下一步缺口。
- 新增 `examples/adoptions/matrix.json`，用稳定英文 keys 记录同一组 pilot。
- 新增 schema 和 evaluator checks，确保 `rclean`、`litellm-rs`、
  `claude-code-monitor` 三个已知 pilot 不会静默丢失。
- `evaluate.py` 输出 adoption matrix 的 checks 和 artifacts。
- 保持所有验证只读，不修改外部 pilot 仓库。

## Non-goals

- 不在本 issue 中把任何外部仓库升级为完整 `repo_integrated`。
- 不新增 GitHub 写操作、自动 label、自动 issue 创建、自动 merge。
- 不让 matrix 依赖聊天记录作为运行时 truth；聊天记录只用于一次性迁移证据。
- 不引入第三方 Python 依赖。

## Users

- `maintainer`: 需要知道 SpecRail 真实验证到哪一层。
- `agent_worker`: 需要在执行任务前知道已有 pilot 证据和缺口。
- `reviewer`: 需要用 deterministic checks 确认 adoption matrix 没有漂移。

## Behavior

### Adoption levels

Matrix 使用这些稳定 level：

- `referenced`
- `smoke`
- `spec_packet`
- `pr_gate`
- `repo_integrated`
- `automation_ready`

每个 repo 只记录当前最强的 verified signal。level 不是承诺成熟度。

### Required pilot entries

Fixture 必须包含：

- `rclean`: current level `smoke`
- `litellm-rs`: current level `pr_gate`
- `claude-code-monitor`: current level `spec_packet`

每条记录必须包含：

- `id`
- `name`
- `repo`
- `current_level`
- `status`
- `evidence`
- `verified_behaviors`
- `next_gap`

### Evaluator

`python3 evaluate.py --repo . --spec-dir specs/GH13 --format json` 必须：

- 验证 workflow/spec/task 基础 artifact。
- 验证 `examples/rclean-smoke.md`。
- 验证 `docs/ADOPTION_MATRIX.md` 和 `examples/adoptions/matrix.json`。
- 对 SpecRail 仓库内 evidence path 执行存在性检查。
- 对外部本地路径和 GitHub URL 只做记录完整性检查，不访问网络、不写外部 repo。

## Acceptance Criteria

- `docs/ADOPTION_MATRIX.md` 存在并列出三类 pilot。
- `examples/adoptions/matrix.json` 存在并包含三个 required pilot IDs。
- `schemas/adoption_matrix.schema.json` 存在并通过 schema 文件基础检查。
- `evaluate.py` 输出包含 adoption matrix artifact 和 checks。
- tests 覆盖缺失 pilot ID 的 failure。
- `python3 checks/check_workflow.py --repo . --spec-dir specs/GH13` 通过。
- `python3 evaluate.py --repo . --spec-dir specs/GH13 --format json` 通过。
- `python3 -m pytest` 通过。
