# GH1434 Tasks

Issue: `#1434`
Product spec: `specs/GH1434/product.md`
Tech spec: `specs/GH1434/tech.md`

## Status

- [ ] `SP1434-T001` Owner: `observe` | Done when: UsageProbe 计数器覆盖 thread/*、turn/* 路由分发点与 thread_manager/task_db/task_executor/task_runner/harness-eval/review_store 公开入口，每日 probe_report 事件可查询，单测覆盖计数与日报 | Verify: `cargo test --package harness-server probe`
- [ ] `SP1434-T002` Owner: `ops` | Done when: `scripts/archive-phase1-data.sh` 对 thread_db.threads、task_db.tasks 及关联表产出 pg_dump 存档 + RESTORE.md，恢复演练到临时库行数一致（threads=9, tasks=62 为当前值，以执行时实际行数为准） | Verify: `bash scripts/archive-phase1-data.sh && test -s archives/phase1-*/RESTORE.md`
- [ ] `SP1434-T003` Owner: `gate` | Done when: 探针运行 ≥7 天且待删面计数为 0，或维护者在 issue #1434 留言豁免并说明理由；结论记录在各 removal PR 描述中 | Verify: probe_report 事件查询输出附在 PR
- [ ] `SP1434-T004` Owner: `workspace` | Done when: harness-eval 移出 workspace members，相关引用清理，eval_store schema 数据原地留档 | Verify: `cargo test --workspace && ! grep -r harness-eval crates/*/Cargo.toml Cargo.toml`
- [ ] `SP1434-T005` Owner: `server` | Done when: review_store 模块及 router/handlers 引用删除 | Verify: `cargo test --workspace && rg -l review_store crates | wc -l 输出 0`
- [ ] `SP1434-T006` Owner: `consumers` | Done when: CLI 子命令、dashboard 面板、websocket 中对 thread/turn/task 端点的引用先行移除或改指 workflow runtime 等价物，dashboard 首屏无回归 | Verify: `cargo test --workspace` + dashboard 手动加载
- [ ] `SP1434-T007` Owner: `server` | Done when: thread/* 与 turn/* 共 16 个 RPC 方法从 protocol 定义、router 注册、handlers 移除，thread_manager/thread_db 模块删除 | Verify: `cargo test --workspace && rg -l 'thread_manager|thread/start|turn/start' crates | wc -l 输出 0`
- [ ] `SP1434-T008` Owner: `server` | Done when: 依赖扫描列出 workflow runtime 对 task_* 的真实引用；被引用部分仅搬运（diff 对照证明无语义改动）并入 runtime 邻近模块，其余 task_db/task_executor/task_runner 删除 | Verify: `cargo test --workspace` + PR 附搬运对照表
- [ ] `SP1434-T009` Owner: `docs` | Done when: CHANGELOG 记录 16 个移除方法的 breaking 清单与归档/恢复指引；净删除 LOC 统计写入汇总 PR（预期 >10k） | Verify: 评审核对 + `git diff --shortstat` 汇总
- [ ] `SP1434-T010` Owner: `tests` | Done when: 全 workspace 测试与 clippy -D warnings 通过，smoke 清单（serve 启动/status/pr fix --help/webhook 健康/dashboard 首屏）逐项通过并附输出 | Verify: `cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings`

## Parallelization

- Lane A (T001, T002): 探针与归档，立即可做，互不依赖。
- Lane B (T004, T005): eval 与 review_store 删除，各自独立 PR，等 T003 门通过。
- Lane C (T006 → T007 → T008): 消费方先行，再删 thread/turn，最后 task 层（依赖最多，压轴）。
- T009/T010 收尾，依赖全部删除 PR 合并。
