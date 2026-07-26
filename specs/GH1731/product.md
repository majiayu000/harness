# Product Spec

## Linked Issue

GH-1731

complexity: high

## User Problem

Harness loads several repository instruction and skill surfaces, but operators
cannot produce a deterministic, reviewable inventory of repository files that
may influence agent behavior. Existing loaders answer narrow execution needs
and do not provide a unified repository-observed record across instructions,
skills, MCP, hooks, memory, policy, workflow, validation, and toolchain
configuration.

The inventory must remain honest: discovery proves that content exists under a
repository root, not that an agent selected, loaded, or executed it.

## Goals

- Discover a documented allowlist of Agent Stack surfaces within one explicit
  repository root.
- Emit typed ASC-001 components with repository-observed trust and selection
  semantics.
- Make paths, file ordering, and content digests deterministic.
- Reject root escape, unreadable discovered content, and incomplete required
  observations visibly.
- Keep global user configuration and runtime probing outside the default scan.

## Non-Goals

- Determining which discovered sources were effective in a runtime prompt.
- Scanning arbitrary user-home, system, PATH, plugin-cache, or sibling
  repository locations.
- Executing hooks, commands, package managers, MCP servers, or runtime probes.
- Computing aggregate stack IDs, comparing inventories, or assigning verdicts.
- Parsing free text for sensitive capabilities; that belongs to ASC-009.
- Adding public CLI commands; native CLI exposure belongs to ASC-026.

## User-Visible Behavior

1. **B-001:** Inventory requires an explicit existing repository root. Harness
   opens that root once and uses the resulting directory handle as the
   observation and authorization boundary for all later access. The inventory
   does not claim or serialize an ambient canonical path for that handle.
   Missing, unreadable, or non-directory roots return an error and produce no
   successful inventory.
2. **B-002:** Default discovery is limited to the repository root. User-global,
   system, sibling, plugin-cache, and executable PATH locations are excluded
   unless a later contract explicitly adds a separately labeled scope.
3. **B-003:** The v0.1 allowlist discovers root instruction files
   `AGENTS.md`, `AGENTS.override.md`, `CLAUDE.md`, `WORKFLOW.md`, and
   `MEMORY.md`; the instruction files `AGENTS.md`, `AGENTS.override.md`, and
   `CLAUDE.md` immediately beneath `src`, `crates`, `lib`, and `pkg`; skill
   roots `.claude/skills`, `.codex/skills`, `.agents/skills`, and `skills`;
   hook entrypoints `.harness/guards/*.sh` and direct children of `.githooks`
   whose basename is a documented Git lifecycle hook; MCP files `.mcp.json` and
   `mcp.json`; policy/validation roots `.vibeguard`, `rules`,
   `requirements.toml`, `.remem`, `remem.toml`, `.harness/config.toml`,
   `.harness/skills`, `.harness/rules`, `.harness/sg`,
   `harness.toml`, `.github/workflows`, and `.cursor/rules`. Skill discovery
   emits direct `*.md` definitions from every skill root and recursively emits
   exact `SKILL.md` package entrypoints from non-Harness skill roots;
   `.harness/skills/*.usage.json`, package references, and other sidecar files
   are not independent stack units. Generic `.claude/hooks` and `hooks`
   directories are not inventoried because directory membership alone does not
   prove lifecycle binding. When repository-root `harness.toml` exists,
   repository-relative `rules.discovery_paths`, `rules.builtin_path`,
   `rules.exec_policy_paths`, and `rules.requirements_path` entries extend the
   policy inventory through the same root capability; absolute sources remain
   outside the repository-scoped result. Toolchain and validation selectors
   include `Cargo.toml`, `go.mod`, `package.json`,
   `pyproject.toml`, `setup.py`, `requirements.txt`, `build.gradle`,
   `build.gradle.kts`, `pom.xml`, root `*.csproj` and `*.sln` files, `Gemfile`,
   `yarn.lock`, `pnpm-lock.yaml`, `.eslintrc`, `.eslintrc.js`,
   `.eslintrc.cjs`, `.eslintrc.json`, `.eslintrc.yaml`, `.eslintrc.yml`,
   `eslint.config.js`, `eslint.config.mjs`, `eslint.config.cjs`, `biome.json`,
   `.rubocop.yml`, the root `spec` directory-presence predicate, `Makefile`,
   and `justfile`. Operational `.harness` paths such as `local`, `drafts`,
   `generated`, and `gc-checkpoint.json` are excluded.
4. **B-004:** A missing allowlisted file or directory means no component for
   that location. Harness does not emit placeholder components, fabricated
   digests, aliases, or warning-only fallback content.
5. **B-005:** Every discovered regular file produces one inventory entry with
   a valid ASC-001 component, a normalized repository-relative
   `/`-separated locator, lowercase SHA-256 content digest,
   `repository_observed` observation and trust, selection state `discovered`,
   and freshness `fresh` because inventory directly read the source in the
   current observation. The entry records the opened file's Unix executable bit
   as `true` or `false` on Unix and explicitly marks it unobserved elsewhere, so
   disabling a hook changes evidence even when its bytes do not. The root
   `spec` predicate emits one typed directory-presence entry with absent
   integrity and freshness `fresh` because the current observation directly
   probed it; inventory does not recursively inspect test content.
6. **B-006:** Component kind comes from the matched allowlist rule, not from
   file contents or directory ancestry alone. Every recursive directory rule
   has a closed surface-specific entry selector, and only matched definition
   entrypoints inherit that rule's kind. A skill usage sidecar, package support
   file, or other unsupported descendant does not become a component merely
   because it is beneath a typed directory; unsupported files outside every
   selector are not inventoried. Repository-relative rule sources derived from
   `harness.toml` are typed `policy`; directory sources select recursive `*.md`
   and `*.toml` definitions, while configured file sources select that exact
   file.
7. **B-007:** Discovery order and output order are deterministic: allowlist
   rule order is stable, selected files and directories required for recursive
   discovery are sorted by normalized relative locator, and filesystem
   enumeration order cannot change output. Unsupported descendants are filtered
   by native basename or extension before portable locator normalization.
8. **B-008:** Traversal and file opens are relative to one capability-scoped
   directory handle, so absolute paths, `..`, symlink swaps, and symlinks whose
   targets escape the repository cannot grant access outside that handle. An
   escaping target or a target that cannot be opened as a regular file returns
   an explicit error. A symlink swapped between valid in-root regular-file
   targets may resolve to either target; the opened file handle is the
   observation authority, the component retains the repository link locator,
   and its digest covers exactly the bytes returned by that handle. A rule that
   expects a file rejects a symlink resolving to a directory; only recursive or
   configured file-or-directory rules may descend through directory symlinks.
9. **B-009:** A selected allowlisted entry that exists but cannot be opened,
   read, resolved, classified, or represented as a lossless UTF-8 portable
   locator returns an error with a failure category and, when representable,
   its safe relative locator; a non-UTF-8 name reports only the nearest
   representable ancestor. Harness does not use lossy path conversion, silently
   omit a selected entry, or report a complete successful inventory. A present
   `harness.toml` with invalid TOML or an invalid, escaping, or missing
   repository-relative rule source fails with a stable typed category rather
   than silently dropping configured policy.
10. **B-010:** Directory traversal is bounded by configured regular-file,
    opened-directory, aggregate-encountered-entry, aggregate-byte, depth,
    entries-per-directory, and per-file byte limits. Selected non-regular
    special files, recursive symlink cycles, invalid limit values, and every
    exceeded limit fail visibly rather than being skipped. On Unix, selected
    file candidates are opened capability-relatively with nonblocking semantics
    before handle metadata is trusted, so a path raced to a FIFO cannot hang
    inventory; other platforms use the corresponding handle-first type check.
11. **B-011:** Repeating inventory against unchanged repository bytes,
    executable metadata, and directory-presence predicates produces the same
    ordered entries and digests regardless of current time or filesystem
    enumeration order.
12. **B-012:** Inventory performs no writes, subprocess execution, network
    calls, hook invocation, MCP connection, package resolution, or modification
    of current prompt-loading behavior.

## Acceptance Criteria

- [ ] The allowlist in B-003 is represented once in typed code with component
      kind and file/directory behavior.
- [ ] Fixture repositories cover every allowlisted surface and prove unrelated
      files are excluded.
- [ ] Hook fixtures prove only `.harness/guards/*.sh` and documented direct Git
      hook basenames become `hook` components; README, helper, and nested
      fixture files are excluded.
- [ ] Config fixtures prove every unique repository-relative
      `(locator, component_kind)` binding loaded from `harness.toml` is
      inventoried once, the same locator retains distinct typed roles, absolute
      sources remain out of scope, and invalid or missing relative sources fail
      typed.
- [ ] Every output validates under the ASC-001 component contract.
- [ ] Tests prove path normalization, stable ordering, content hashing,
      missing-entry behavior, root-handle containment across path replacement,
      escaping-symlink rejection, in-root symlink-swap semantics, cycles,
      non-UTF-8 paths, special files, unreadable files, and every resource
      limit.
- [ ] Tests prove all `lang_detect.rs` selectors are inventoried, root suffix
      rules stay root-scoped, the `spec` predicate does not recurse, and
      executable-bit changes alter hook evidence without changing content
      digests.
- [ ] Capability-scoped filesystem traversal is introduced through one audited,
      narrowly owned dependency; the already-locked `libc` crate supplies only
      Unix `O_NONBLOCK`, and existing workspace SHA-256 support is reused.
- [ ] The implementation exposes a library service for later CLI and runtime
      consumers but adds no new command in this issue.

## Boundary Checklist

| Boundary | Verdict |
| --- | --- |
| Empty / missing input | Covered by B-001 and B-004. |
| Error and failure paths | Covered by B-001, B-008, B-009, and B-010. |
| Authorization / permission | Covered by B-001, B-002, and B-012; the caller selects one readable root and inventory gains no external authority. |
| Concurrency / race / ordering | Covered by B-007, B-008, and B-011. Handle-relative opens prevent path/symlink races from escaping the root. The scan is non-atomic: an in-root symlink swap may select either valid target and in-place writes may change bytes during a read, so callers requiring a stable snapshot must provide a quiescent repository. |
| Retry / repetition / idempotency | Covered by B-011 and B-012. |
| Illegal state transitions | N/A. Inventory is a stateless read operation. |
| Compatibility / migration | Covered by B-003, B-005, and B-012; no existing persisted representation changes. |
| Degradation / fallback | Covered by B-004 and B-009; missing is blank, while unreadable existing data is an error. |
| Evidence and audit integrity | Covered by B-005 through B-011. |
| Cancellation / interruption / partial completion | Covered by B-009 and B-012; interruption cannot publish a successful partial inventory or mutate the repository. |

## Edge Cases

- The repository root itself is a symlink.
- An allowlisted directory contains a symlink back to an ancestor.
- Two safe in-root symlinks target the same file.
- A file is removed or modified between metadata inspection and reading.
- An allowlisted path exists as a directory where a file is expected.
- A skill directory contains usage sidecars, package references, binary files,
  or special files that are not selected definition entrypoints.
- A generic hook directory contains README, helper, and fixture files with no
  lifecycle binding.
- A selected regular file is replaced by a FIFO immediately before open.
- `harness.toml` declares duplicate, absolute, escaping, missing, file, and
  directory rule sources.
- An exact-file rule resolves through a symlink to a directory.
- `Makefile` exists while `makefile` does not; matching remains exact.
- The repository has no allowlisted surfaces.

## Rollout Notes

The service is additive and has no user-facing command until ASC-026. Initial
consumers should present it explicitly as repository-observed inventory.
Changing the allowlist later changes inventory semantics and must be reviewed
as a schema/behavior change rather than silently broadening discovery.
