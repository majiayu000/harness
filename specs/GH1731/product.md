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
   canonicalizes it once before discovery; missing, unreadable, or non-directory
   roots return an error and produce no successful inventory.
2. **B-002:** Default discovery is limited to the repository root. User-global,
   system, sibling, plugin-cache, and executable PATH locations are excluded
   unless a later contract explicitly adds a separately labeled scope.
3. **B-003:** The v0.1 allowlist discovers root instruction files
   `AGENTS.md`, `AGENTS.override.md`, `CLAUDE.md`, `WORKFLOW.md`, and
   `MEMORY.md`; the instruction files `AGENTS.md`, `AGENTS.override.md`, and
   `CLAUDE.md` immediately beneath `src`, `crates`, `lib`, and `pkg`; skill
   roots `.claude/skills`, `.codex/skills`, `.agents/skills`, and `skills`;
   hook roots `.claude/hooks`, `hooks`, and `.githooks`; MCP files `.mcp.json`
   and `mcp.json`; policy/validation roots `.vibeguard`, `rules`,
   `requirements.toml`, `.remem`, `remem.toml`, `.harness`, `harness.toml`,
   `.github/workflows`, and `.cursor/rules`; and toolchain files `Cargo.toml`,
   `package.json`, `pyproject.toml`, `Makefile`, and `justfile`.
4. **B-004:** A missing allowlisted file or directory means no component for
   that location. Harness does not emit placeholder components, fabricated
   digests, aliases, or warning-only fallback content.
5. **B-005:** Every discovered regular file produces one ASC-001 component
   with a normalized repository-relative `/`-separated locator, lowercase
   SHA-256 content digest, `repository_observed` observation and trust, and
   selection state `discovered`.
6. **B-006:** Component kind comes from the matched allowlist rule, not from
   file contents. Files beneath a typed directory inherit that rule's kind;
   unsupported files outside every allowlist rule are not inventoried.
7. **B-007:** Discovery order and output order are deterministic: allowlist
   rule order is stable, directory entries are sorted by normalized relative
   locator, and filesystem enumeration order cannot change output.
8. **B-008:** Traversal and file opens are relative to one capability-scoped
   directory handle, so absolute paths, `..`, symlink swaps, and symlinks whose
   targets escape the repository cannot grant access outside that handle. An
   escaping or raced path returns an explicit error; a safe in-root symlink is
   represented by its repository locator and hashes the opened regular-file
   content once.
9. **B-009:** An allowlisted entry that exists but cannot be opened, read,
   resolved, classified, or represented as a lossless UTF-8 portable locator
   returns an error with a failure category and, when representable, its safe
   relative locator; a non-UTF-8 name reports only the nearest representable
   ancestor. Harness does not use lossy path conversion, silently omit the
   entry, or report a complete successful inventory.
10. **B-010:** Directory traversal is bounded by configured file-count,
    aggregate-byte, depth, entries-per-directory, and per-file byte limits.
    Non-regular special files, recursive symlink cycles, invalid limit values,
    and every exceeded limit fail visibly rather than being skipped.
11. **B-011:** Repeating inventory against unchanged repository bytes produces
    the same ordered components and digests regardless of current time or
    filesystem enumeration order.
12. **B-012:** Inventory performs no writes, subprocess execution, network
    calls, hook invocation, MCP connection, package resolution, or modification
    of current prompt-loading behavior.

## Acceptance Criteria

- [ ] The allowlist in B-003 is represented once in typed code with component
      kind and file/directory behavior.
- [ ] Fixture repositories cover every allowlisted surface and prove unrelated
      files are excluded.
- [ ] Every output validates under the ASC-001 component contract.
- [ ] Tests prove path normalization, stable ordering, content hashing,
      missing-entry behavior, root escape and symlink-race rejection, cycles,
      non-UTF-8 paths, special files, unreadable files, and every resource
      limit.
- [ ] Capability-scoped filesystem traversal is introduced as one audited,
      narrowly owned dependency; existing workspace SHA-256 support is reused.
- [ ] The implementation exposes a library service for later CLI and runtime
      consumers but adds no new command in this issue.

## Boundary Checklist

| Boundary | Verdict |
| --- | --- |
| Empty / missing input | Covered by B-001 and B-004. |
| Error and failure paths | Covered by B-001, B-008, B-009, and B-010. |
| Authorization / permission | Covered by B-001, B-002, and B-012; the caller selects one readable root and inventory gains no external authority. |
| Concurrency / race / ordering | Covered by B-007, B-008, and B-011. Handle-relative opens prevent path/symlink races from escaping the root. The scan is non-atomic: in-place writes may change bytes during a read, so callers requiring a stable snapshot must provide a quiescent repository. |
| Retry / repetition / idempotency | Covered by B-011 and B-012. |
| Illegal state transitions | N/A. Inventory is a stateless read operation. |
| Compatibility / migration | Covered by B-003, B-005, and B-012; no existing persisted representation changes. |
| Degradation / fallback | Covered by B-004 and B-009; missing is blank, while unreadable existing data is an error. |
| Evidence and audit integrity | Covered by B-005 through B-11. |
| Cancellation / interruption / partial completion | Covered by B-009 and B-012; interruption cannot publish a successful partial inventory or mutate the repository. |

## Edge Cases

- The repository root itself is a symlink.
- An allowlisted directory contains a symlink back to an ancestor.
- Two safe in-root symlinks target the same file.
- A file is removed or modified between metadata inspection and reading.
- An allowlisted path exists as a directory where a file is expected.
- A skill directory contains binary or special files.
- `Makefile` exists while `makefile` does not; matching remains exact.
- The repository has no allowlisted surfaces.

## Rollout Notes

The service is additive and has no user-facing command until ASC-026. Initial
consumers should present it explicitly as repository-observed inventory.
Changing the allowlist later changes inventory semantics and must be reviewed
as a schema/behavior change rather than silently broadening discovery.
