# Task Plan

## Linked Issue

GH-1686

## Spec Packet

- Product: `specs/GH1686/product.md`
- Tech: `specs/GH1686/tech.md`

## Implementation Tasks

- [ ] `SP1686-T1` Owner: Codex | Done when: exact-base workflow inventory, upstream resolver behavior, duplicate search, failure reproduction, Bun release availability, and local baseline are recorded | Verify: commands in T1 below
- [ ] `SP1686-T2` Owner: Codex | Done when: the root version pin and all three workflow references are implemented with exact scope and Bun 1.3.14 passes web validation | Verify: commands in T2 below
- [ ] `SP1686-T3` Owner: Codex | Done when: workflow, Rust, exact-head CI, review-thread, and SpecRail gates pass for the separate implementation PR and cleanup is verified | Verify: commands in T3 below

### SP1686-T1 — Capture the resolver and CI baseline

- Owner: Codex implementation agent.
- Dependencies: merged GH-1686 specification PR and a fresh worktree from
  current `origin/main`.
- Covers: B-001 through B-007.
- Work:
  - record the exact base SHA and all active `setup-bun` inputs;
  - confirm no version file, duplicate issue, or overlapping open PR exists;
  - preserve both failed setup logs and the upstream exact-version resolver
    evidence;
  - verify the Bun 1.3.14 release asset and run the current local web baseline;
  - run manual VibeGuard baseline review and stop for any unrelated red build.
- Done when:
  - the four-file implementation scope and strict-version behavior are
    unambiguous;
  - project web commands pass independently of the failing `latest` resolver;
  - no hidden duplicate or baseline failure remains.
- Verify:
  - `git rev-parse HEAD`;
  - searches in `tech.md` for active setup inputs and version files;
  - failed Actions job logs for run `29539326830`;
  - upstream `setup-bun` strict-version resolver inspection;
  - current web frozen install, typecheck, test, and build commands.

### SP1686-T2 — Pin and verify the Bun toolchain

- Owner: Codex implementation agent.
- Dependencies: SP1686-T1.
- Covers: B-001 through B-006.
- Work:
  - add `.bun-version` with exact value `1.3.14`;
  - replace all three active `bun-version: latest` inputs with the shared file;
  - add `.bun-version` to the main CI `ci` filter and Web CI paths so a
    version-only change validates the selected toolchain;
  - preserve every unrelated workflow line and all manifests/lockfiles;
  - install or download Bun 1.3.14 and run the complete web validation surface;
  - prove exact version/reference counts and approved four-file scope.
- Done when:
  - Bun 1.3.14 is the single active workflow version source;
  - a `.bun-version`-only change selects main CI and Web CI;
  - all web and SDK checks pass with frozen dependencies;
  - workflow YAML parses and unrelated configuration is unchanged.
- Verify:
  - exact `.bun-version` byte check;
  - active-workflow input inventory, zero-`latest` assertion, and exact path
    filter/trigger inventory;
  - `bun --version` under the pinned binary;
  - web frozen install, typecheck, test, and build commands;
  - YAML parse, diff, manifest/lockfile, and four-file scope checks.

### SP1686-T3 — Prove PR readiness and clean up

- Owner: Codex implementation agent; human final review remains authoritative.
- Dependencies: SP1686-T2.
- Covers: B-001 through B-008.
- Work:
  - compare the implementation with the issue and complete spec packet;
  - run the deterministic local verification matrix at final head;
  - open the separate implementation PR with root cause, plan, exact resolver
    evidence, scope, risks, verification, and rollback;
  - wait for exact-head CI and Gemini review, address valid findings, audit all
    review threads, run the required SpecRail PR gate, and merge only under the
    standing authorization;
  - rerun or update dependent PRs only after the fix is present on `main`;
  - remove worktrees and temporary downloaded artifacts.
- Done when:
  - B-001 through B-008 have fresh evidence;
  - local and exact-head remote gates pass with no unresolved active thread;
  - authorized merge, issue closure, dependent-PR recovery, and cleanup are
    verified.
- Verify:
  - version/reference/scope, web, format, all-features, workspace Clippy/test,
    SpecRail, exact-head CI, review-thread, and required PR gate commands from
    this packet.

## Parallelization

No parallel writable lanes are planned. The version file and three workflow
references are one atomic toolchain selection change and will be implemented,
verified, and committed sequentially.

## Verification

- [ ] Global and GH-1686 SpecRail checks pass.
- [ ] Product behaviors B-001 through B-008 map to tasks and verification.
- [ ] The strict semantic version bypasses tag enumeration in upstream source.
- [ ] One version file and all three active workflow references are exact.
- [ ] Bun 1.3.14 web/SDK validation and repository Rust gates pass.
- [ ] VibeGuard, exact-head CI, review-thread, and PR gates pass.

## Handoff Notes

- Exact issue/spec base: `807e129e746bb95afa9cba896d1c1916e7e6ea35`.
- Failure evidence: PR #1685 run `29539326830`, setup jobs `87757987147` and
  `87758451029`, both HTTP 503 before project commands.
- Upstream `setup-bun` uses direct release URL construction only for strict
  versions; ranges and `latest` enumerate tags.
- Preserve historical documentation examples and all workflow behavior outside
  the three input substitutions and two `.bun-version` path additions.
- Route any web, SDK, or Rust product finding to a separate issue/spec cycle.
