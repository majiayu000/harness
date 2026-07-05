# Tech Spec

## Linked Issue

GH-1459

## Product Spec

See `specs/GH1459/product.md`.

## Current System

| Area | Files | Current behavior | Why relevant |
| --- | --- | --- | --- |
| Workspace manifest | `Cargo.toml` | The root manifest has no `[profile.dev]` or `[profile.test]` section. | Cargo defaults to full debuginfo for dev/test builds. |
| Test builds | workspace crates under `crates/` | `cargo test --workspace --lib` links many test binaries using the inherited dev profile. | Link time and artifact size are the direct user problem. |
| Local target dir | `target/debug` | The issue reports very large accumulated `deps` and `incremental` artifacts. | Size evidence should be captured before and after the profile change. |
| Debugging behavior | panic output and backtraces | Full debuginfo is more detailed than line tables. | The implementation must prove panic diagnostics still include file-line data. |
| GH-1458 coordination | `Cargo.toml`, hooks, workflows | GH-1458 changes lint policy in the same manifest. | A combined implementation avoids two separate full rebuilds. |

## Proposed Design

1. Add the dev profile override to the root `Cargo.toml`:

   ```toml
   [profile.dev]
   debug = "line-tables-only"
   ```

2. Evaluate dependency debuginfo stripping in the implementation PR:

   ```toml
   [profile.dev.package."*"]
   debug = false
   ```

   Include it only if local verification shows a clear size or link-time benefit
   without unacceptable diagnostics loss. The PR body must record the decision.

3. Rely on `profile.test` inheritance from `profile.dev` unless verification
   shows test builds require an explicit test profile. Do not add a separate
   `profile.test` setting without evidence.

4. Verify panic diagnostics by temporarily forcing a controlled test panic or
   assertion failure, running the focused test command with backtraces enabled,
   inspecting file-line output, and reverting the temporary failure before
   commit.

5. Collect size evidence with `du -sh target/debug` before and after a clean
   rebuild. If the local environment cannot afford a full clean rebuild, record
   that limitation and provide the strongest available partial evidence.

## Data Flow

Cargo applies the dev profile to workspace dev builds and to test builds through
the default profile inheritance. The produced artifacts retain line tables for
source locations while omitting full variable-level debuginfo. Test and check
commands use the same source code paths; only compiler profile settings change.

## Alternatives Considered

- Keep full debuginfo by default. Rejected because it preserves the reported
  target-dir and link-time cost for every normal agent loop.
- Set `debug = false` for all dev builds. Rejected because panic backtraces and
  file-line diagnostics are an important default workflow surface.
- Add only dependency stripping without line tables for workspace crates.
  Rejected as the primary change because it does not address workspace test
  binary debug-info cost as directly.
- Add a separate `profile.test`. Rejected unless verification shows inheritance
  is insufficient.

## Risks

- Security: no direct security surface; this changes compiler artifact detail.
- Compatibility: interactive debugger sessions may lose variable inspection
  detail. Developers can override locally for deep debugging.
- Performance: the first profile change causes an expected rebuild. Subsequent
  dev/test builds should link and store less debug data.
- Maintenance: dependency debuginfo stripping may make third-party stack frames
  less informative; the implementation PR must make that tradeoff explicit.

## Test Plan

- [ ] Build profile audit: root `Cargo.toml` contains
      `[profile.dev] debug = "line-tables-only"`.
- [ ] Optional dependency profile decision recorded in the PR body and, if
      enabled, present under `[profile.dev.package."*"]`.
- [ ] `cargo test --workspace --lib` passes, using documented Postgres
      configuration or existing skips where applicable.
- [ ] Backtrace check: a temporary failing test or panic still prints file-line
      information with the new profile, and the temporary change is reverted.
- [ ] Size check: record `du -sh target/debug` before and after a clean rebuild
      or record a clear limitation.
- [ ] SpecRail checks: `python3 checks/check_workflow.py --repo .
      --spec-dir specs/GH1459` and `python3 checks/check_workflow.py --repo .`
      pass.

## Rollback Plan

Revert the `[profile.dev]` and optional dependency-profile changes. This
restores full dev/test debuginfo and does not require data migration.
