# GH1430 Tasks

Issue: `#1430`
Product spec: `specs/GH1430/product.md`
Tech spec: `specs/GH1430/tech.md`

## Status

- [ ] `SP1430-T001` Owner: `runtime-worker` | Done when: `classify_agent_failure` pure function exists with the regex table from tech.md,每类带取自生产数据的 fixture 串正例 + unclassified 兜底负例 | Verify: `cargo test --package harness-server classify`
- [ ] `SP1430-T002` Owner: `runtime-worker` | Done when: executor 在失败落库时把 `failure_class` 写入 runtime_jobs 的 `data` jsonb，成功路径不写 | Verify: `cargo test --package harness-server workflow_runtime_worker`
- [ ] `SP1430-T003` Owner: `runtime-worker` | Done when: `circuit_breaker.rs` 状态机（Closed/Open/HalfOpen）实现按 (profile, class) 连续计数、成功清零、冷却退避（×2 至上限），全部转移有单测 | Verify: `cargo test --package harness-server circuit_breaker`
- [ ] `SP1430-T004` Owner: `runtime-worker` | Done when: worker tick 派发前调用 `allow(profile)`，Open 未到期跳过该 profile 并顺延 `not_before`；in-flight job 不受影响；结果落库后调用 record_success/record_failure | Verify: `cargo test --package harness-server workflow_runtime_worker`
- [ ] `SP1430-T005` Owner: `observe` | Done when: 熔断触发/关闭/复位均发 error 级（触发）与 info 级（恢复）事件，字段含 profile、class、consecutive、cooldown_until | Verify: `cargo test --package harness-server`
- [ ] `SP1430-T006` Owner: `config` | Done when: `[workflow.circuit_breaker]`（enabled/consecutive_failures/cooldown_secs/backoff_factor/max_cooldown_secs）解析、默认开启、default.toml.example 记录 | Verify: `cargo test --package harness-core config`
- [ ] `SP1430-T007` Owner: `server-api` | Done when: status 聚合 JSON 增加 `circuit_breakers` 数组，CLI text 输出在存在非 Closed 熔断器时显示一行 | Verify: `cargo test --package harness-server && ./target/release/harness status --json`
- [ ] `SP1430-T008` Owner: `server-api` | Done when: 手动复位入口（JSON-RPC method 或 `harness runtime breaker reset <profile>`）存在且复位留事件 | Verify: `cargo test --package harness-server`
- [ ] `SP1430-T009` Owner: `tests` | Done when: 风暴回放集成测试以 06-09 序列形状（同类失败连续到达）seed，断言第 5 个失败后该 profile 停止派发，冷却期内仅 half-open 探测放行 | Verify: `cargo test --package harness-server storm_replay`
- [ ] `SP1430-T010` Owner: `tests` | Done when: 全 workspace 测试通过 | Verify: `cargo test --workspace`

## Parallelization

- Lane A (T001, T002): 失败分类与落库。
- Lane B (T003, T005, T006): 状态机、事件、配置（不依赖 Lane A）。
- Lane C (T004, T007, T008, T009): 接入派发路径与暴露面（依赖 A+B）。
