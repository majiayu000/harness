# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.6.34](https://github.com/majiayu000/harness/releases/tag/harness-workflow-v0.6.34) - 2026-06-19

### Added

- *(web)* add operator monitor cockpit ([#1328](https://github.com/majiayu000/harness/pull/1328))
- *(runtime)* gate PR readiness through quality gate
- *(tasks)* paginate task list queries
- add tiered review fallback for review loop ([#1042](https://github.com/majiayu000/harness/pull/1042))
- *(workflow)* add Ready-for-Merge human gate ([#935](https://github.com/majiayu000/harness/pull/935)) ([#939](https://github.com/majiayu000/harness/pull/939))
- *(db)* migrate remaining SQLite stores to Postgres (closes #747) ([#831](https://github.com/majiayu000/harness/pull/831))
- extract harness-workflow crate with circuit_breaker, checkpoint, plan_db, task_queue ([#662](https://github.com/majiayu000/harness/pull/662))

### Fixed

- *(workflow)* constrain runtime job statuses ([#1376](https://github.com/majiayu000/harness/pull/1376))
- *(storage)* make path schema openings explicit ([#1364](https://github.com/majiayu000/harness/pull/1364))
- *(runtime)* type workflow command status writes ([#1362](https://github.com/majiayu000/harness/pull/1362))
- *(runtime)* allow blocked reconciliation completion ([#1330](https://github.com/majiayu000/harness/pull/1330))
- *(runtime)* compact runtime tree polling ([#1327](https://github.com/majiayu000/harness/pull/1327))
- *(runtime)* block unknown successful completions ([#1325](https://github.com/majiayu000/harness/pull/1325))
- *(runtime)* commit submissions atomically ([#1322](https://github.com/majiayu000/harness/pull/1322))
- *(runtime)* make child workflow replay idempotent ([#1232](https://github.com/majiayu000/harness/pull/1232))
- *(runtime)* expose running lease states ([#1313](https://github.com/majiayu000/harness/pull/1313))
- *(runtime)* address remaining review followups ([#1244](https://github.com/majiayu000/harness/pull/1244))
- *(workflow)* report ignored legacy merge approvals ([#1302](https://github.com/majiayu000/harness/pull/1302))
- *(runtime)* require issue plans and PR repair evidence ([#1252](https://github.com/majiayu000/harness/pull/1252))
- *(runtime)* require PR repair evidence snapshots ([#1250](https://github.com/majiayu000/harness/pull/1250))
- bound Postgres schema growth (wire orphan reaper) + surface task failure reasons ([#1220](https://github.com/majiayu000/harness/pull/1220))
- *(runtime-host)* claim workflow jobs through workflow runtime ([#1219](https://github.com/majiayu000/harness/pull/1219))
- *(runtime)* extend repo backlog timeout ([#1169](https://github.com/majiayu000/harness/pull/1169))
- *(runtime)* honor fresh PR feedback activity ([#1194](https://github.com/majiayu000/harness/pull/1194))
- *(runtime)* validate replayed events at event time ([#1188](https://github.com/majiayu000/harness/pull/1188))
- *(runtime)* stop queued jobs when worker is disabled ([#1187](https://github.com/majiayu000/harness/pull/1187))
- *(runtime)* require prompt task validation evidence ([#1186](https://github.com/majiayu000/harness/pull/1186))
- *(runtime)* accept legacy repo backlog dispatch decisions ([#1176](https://github.com/majiayu000/harness/pull/1176))
- *(runtime)* persist prompt payloads across restarts ([#1174](https://github.com/majiayu000/harness/pull/1174))
- *(runtime)* prioritize non-backlog jobs ([#1173](https://github.com/majiayu000/harness/pull/1173))
- *(runtime)* address unresolved P1 review followups ([#1163](https://github.com/majiayu000/harness/pull/1163))
- *(runtime)* enforce quality gate success contract ([#1135](https://github.com/majiayu000/harness/pull/1135))
- *(runtime)* cancel dispatching commands ([#1143](https://github.com/majiayu000/harness/pull/1143))
- *(runtime)* validate inline BindPr payloads ([#1137](https://github.com/majiayu000/harness/pull/1137))
- *(runtime)* release closed blocked issue dependencies ([#1134](https://github.com/majiayu000/harness/pull/1134))
- *(workflow)* commit runtime completion atomically
- *(workflow)* harden runtime state transitions
- *(workflow)* separate feedback claims from active PR tasks ([#1037](https://github.com/majiayu000/harness/pull/1037))
- *(workflow)* sweeper uses repair_project_id to fix stale PK on path correction ([#961](https://github.com/majiayu000/harness/pull/961))
- *(workflow)* use canonical project root for IssueWorkflowInstance project_id ([#954](https://github.com/majiayu000/harness/pull/954))
- *(workflow)* sweeper path recovery and warn deduplication ([#953](https://github.com/majiayu000/harness/pull/953))
- *(db)* replace DefaultHasher with SHA-256 for stable Postgres schema names ([#869](https://github.com/majiayu000/harness/pull/869))
- *(server)* prevent HTTP listener deadlock under queue saturation ([#863](https://github.com/majiayu000/harness/pull/863))
- remove per-PR version bump rule and add PR management docs

### Other

- migrate plan_db to shared schema ([#1344](https://github.com/majiayu000/harness/pull/1344))
- Add workflow PR scope guard ([#1334](https://github.com/majiayu000/harness/pull/1334))
- Fix runtime submission id task route lookup ([#1331](https://github.com/majiayu000/harness/pull/1331))
- Add runtime PR hygiene sweep ([#1321](https://github.com/majiayu000/harness/pull/1321))
- Require complete review-thread snapshots for PR readiness ([#1318](https://github.com/majiayu000/harness/pull/1318))
- Harden workflow command status boundaries ([#1314](https://github.com/majiayu000/harness/pull/1314))
- *(workflow)* remove unreachable state variants ([#1312](https://github.com/majiayu000/harness/pull/1312))
- Fix runtime PR feedback with server-owned snapshots ([#1254](https://github.com/majiayu000/harness/pull/1254))
- Fix runtime PR feedback local review gate ([#1224](https://github.com/majiayu000/harness/pull/1224))
- *(release)* v0.6.34 ([#1196](https://github.com/majiayu000/harness/pull/1196))
- Fix atomic PR feedback runtime transitions ([#1148](https://github.com/majiayu000/harness/pull/1148))
- *(runtime)* paginate workflow runtime tree
- Fix dashboard runtime metrics
- Route open PR feedback through runtime prompts
- Fix runtime worker capacity under long turns
- Allow runtime backlog and feedback recovery
- Repair workflow runtime PR recovery
- Fix workflow runtime workspace and Codex recovery ([#1062](https://github.com/majiayu000/harness/pull/1062))
- Decouple workflow runtime from legacy task fallbacks ([#1060](https://github.com/majiayu000/harness/pull/1060))
- Route prompt submissions through workflow runtime ([#1053](https://github.com/majiayu000/harness/pull/1053))
- Route PR feedback through child workflow runtime ([#1052](https://github.com/majiayu000/harness/pull/1052))
- Route repo sprint planning through workflow runtime ([#1051](https://github.com/majiayu000/harness/pull/1051))
- Route repo backlog polling through workflow runtime ([#1050](https://github.com/majiayu000/harness/pull/1050))
- Route PR feedback sweeps through workflow runtime ([#1049](https://github.com/majiayu000/harness/pull/1049))
- Shadow issue submissions in workflow runtime
- Add workflow runtime activity result contracts ([#1047](https://github.com/majiayu000/harness/pull/1047))
- Honor registry metadata for path-based project execution ([#1044](https://github.com/majiayu000/harness/pull/1044))
- Improve Postgres startup health reporting
- Fix workflow migration backfill overwrite ([#1014](https://github.com/majiayu000/harness/pull/1014))
- Guard workflow project-id repair against canonical overwrite ([#993](https://github.com/majiayu000/harness/pull/993))
- Resolve database URL from Harness config ([#995](https://github.com/majiayu000/harness/pull/995))
- *(persistence)* audit all 4 UUID workspace path surfaces ([#968](https://github.com/majiayu000/harness/pull/968)) ([#977](https://github.com/majiayu000/harness/pull/977))
- Move workflow coordination under database authority ([#925](https://github.com/majiayu000/harness/pull/925))
- Make issue workflows the orchestration authority ([#913](https://github.com/majiayu000/harness/pull/913))
- Separate background review load from issue intake capacity
- Respect server.database_url over DATABASE_URL fallback ([#903](https://github.com/majiayu000/harness/pull/903))
- *(db)* convert exec_plans.data to JSONB and add GIN index ([#861](https://github.com/majiayu000/harness/pull/861))
- *(db)* validate schema name before SQL interpolation ([#862](https://github.com/majiayu000/harness/pull/862))
- unify project identity via canonical path across registry, queue, and config ([#825](https://github.com/majiayu000/harness/pull/825))
