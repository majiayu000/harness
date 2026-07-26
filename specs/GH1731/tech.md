# Tech Spec

## Linked Issue

GH-1731

## Product Spec

See `specs/GH1731/product.md`.

<!-- specrail-planned-changes
{"issue":1731,"complete":true,"paths":["crates/harness-core/src/stack/inventory.rs","crates/harness-core/src/stack/inventory_tests.rs","crates/harness-core/src/stack/mod.rs"],"spec_refs":["B-001","B-002","B-003","B-004","B-005","B-006","B-007","B-008","B-009","B-010","B-011","B-012"]}
-->

## Current System

- `crates/harness-core/src/agents_md.rs:5-49` loads and merges global,
  repository, and selected subdirectory instruction files into one prompt
  string. It intentionally discards per-file digest and provenance.
- `crates/harness-core/src/agents_md.rs:52-69` exposes instruction discovery
  candidates, but includes user-global paths and covers only AGENTS/CLAUDE
  files, so it cannot serve as the repository-only stack inventory.
- `crates/harness-skills/src/store.rs:115-131` keeps skill discovery paths and
  persisted skill state, while `store.rs:22-45` tracks skill content hashes.
  The store is an execution/governance subsystem, not a general repository
  file inventory.
- `crates/harness-core/src/lang_detect.rs:150-176` demonstrates bounded
  standard-library root enumeration but is language-specific and fail-soft.
- `crates/harness-core/src/capability.rs:11-49` canonicalizes scoped paths for
  enforcement; inventory requires a separate read-only root containment
  helper and must not change capability-token behavior.
- ASC-001 will add `crates/harness-core/src/stack/mod.rs` with the typed
  component contract used by this issue.
- `crates/harness-core/Cargo.toml:25-30` already includes sha2 and tempfile, so
  discovery and fixture tests require no dependency change.

## Proposed Design

### Inventory Service

Add `stack::inventory` with:

- `AgentStackInventoryOptions { root, max_file_bytes }`;
- `AgentStackInventory`;
- `AgentStackInventoryError`;
- a private `InventoryRule` table;
- `inventory_repository_stack(options) -> Result<AgentStackInventory, ...>`.

`stack/mod.rs` exposes the service and its public option/result/error types. It
does not expose traversal helpers or mutable rule tables.

The result contains the canonical root and ordered ASC-001 components. The root
is observation metadata and is not represented as a component. This issue does
not add an aggregate digest or snapshot identity.

### Closed Discovery Table

Define the B-003 allowlist as one constant typed table. Every row has:

- exact repository-relative path;
- expected entry class (`file` or recursively scanned `directory`);
- `AgentStackComponentKind`.

Root instruction names map to `instructions`, workflow files and
`.github/workflows` map to `workflow`, skill roots map to `skill`, hook roots
map to `hook`, MCP files map to `mcp_server`, memory/remem surfaces map to
`memory`, policy/rule surfaces map to `policy`, Harness configuration maps to
`validation`, and package/toolchain files map to `validation`.

The table order is stable but final output is sorted by normalized locator, so
adding a rule cannot reorder unrelated existing components.

### Safe Traversal

Canonicalize the requested root and require directory metadata. For each rule:

1. inspect with `symlink_metadata`;
2. treat `NotFound` as absence;
3. return a typed error for other metadata failures;
4. canonicalize existing entries and verify `starts_with(canonical_root)`;
5. visit directories with `std::fs::read_dir`, collect entries, normalize, and
   sort before recursion;
6. track canonical directory identities in a set to reject cycles;
7. accept only regular files or safe symlinks resolving to regular files;
8. read through `File::take(max_file_bytes + 1)` and reject overflow;
9. calculate SHA-256 from the exact bytes read.

Safe in-root symlinks retain their link locator as component identity while
hashing target bytes. Duplicate target content may therefore produce multiple
components with distinct repository locators, which accurately reflects
multiple declared stack entry points.

Errors carry a stable category and sanitized repository-relative locator.
They do not include file bytes, resolved out-of-root locations, or arbitrary OS
error strings in serialized evidence.

### Component Construction

Each file becomes an ASC-001 component:

- `schema_version`: ASC-001 constant;
- component ID: `repository:<normalized locator>`;
- source scope: `repository`;
- source locator: normalized locator;
- kind: rule kind;
- digest: validated SHA-256;
- observation/trust: `repository_observed`;
- selection: `discovered`;
- freshness: `unknown`;
- capabilities: empty.

Construction calls ASC-001 validation. Any validation failure becomes an
inventory error; the service never skips an invalid component.

### Test Layout

Keep production traversal in `inventory.rs` and table-driven fixtures in
`inventory_tests.rs`. Tests use tempfile and explicit permissions where the
platform supports unreadable-file assertions. Platform-specific inability to
create an unreadable file must use a deterministic injected reader failure
fixture rather than silently skip the behavior.

## Data Flow

Explicit root/options → canonical root validation → fixed rule lookup → safe,
sorted traversal → bounded byte read → SHA-256 → validated ASC-001 component →
ordered `AgentStackInventory`.

Any existing-entry failure aborts the operation. Missing allowlisted entries
produce no component. No subprocess, network, persistence, or repository write
occurs.

## Product-to-Test Mapping

| Behavior invariant | Implementation area | Verification |
| --- | --- | --- |
| B-001 | options/root preflight | `cargo test -p harness-core stack::inventory_tests::inventory_rejects_missing_file_and_unreadable_roots` |
| B-002 | root-only rule table | `cargo test -p harness-core stack::inventory_tests::inventory_never_reads_user_global_or_sibling_paths` |
| B-003 | `InventoryRule` constant | `cargo test -p harness-core stack::inventory_tests::inventory_discovers_every_v0_1_allowlisted_surface` |
| B-004 | NotFound handling | `cargo test -p harness-core stack::inventory_tests::missing_allowlisted_entries_emit_no_placeholders` |
| B-005 | component construction and hashing | `cargo test -p harness-core stack::inventory_tests::discovered_files_emit_valid_repository_observed_components` |
| B-006 | typed rule classification | `cargo test -p harness-core stack::inventory_tests::component_kind_comes_from_matching_rule` |
| B-007 | sorted traversal/output | `cargo test -p harness-core stack::inventory_tests::filesystem_enumeration_order_does_not_change_inventory` |
| B-008 | canonical containment and symlink handling | `cargo test -p harness-core stack::inventory_tests::inventory_rejects_escaping_symlinks_and_cycles` |
| B-009 | typed read/classification errors | `cargo test -p harness-core stack::inventory_tests::existing_unreadable_entries_fail_inventory` |
| B-010 | special-file and size limits | `cargo test -p harness-core stack::inventory_tests::inventory_rejects_oversized_and_special_entries` |
| B-011 | repeated fixture comparison | `cargo test -p harness-core stack::inventory_tests::unchanged_repository_inventory_is_repeatable` |
| B-012 | read-only scope and existing loaders | `git diff --name-only origin/main...HEAD`; `cargo test -p harness-core agents_md` |

## Alternatives Considered

- Extend `agents_md::discovery_paths`: rejected because it intentionally
  includes global configuration and only instruction files.
- Drive discovery from `SkillStore`: rejected because non-skill stack
  components and repository-only trust do not belong to skill governance.
- Recursively scan the complete repository: rejected for performance, privacy,
  generated-file noise, and false claims about behavioral relevance.
- Use `walkdir` or ignore-file dependencies: rejected because the fixed
  allowlist can be safely traversed with the standard library.
- Continue after unreadable files: rejected because a partial inventory would
  look complete and violate fail-closed evidence semantics.

## Risks

- Security: path escape and symlink races could read outside the requested
  root. Canonical containment is checked for every existing entry immediately
  before opening.
- Logic: an incomplete allowlist omits behavior-affecting sources. The product
  contract and table test enumerate the exact v0.1 surface.
- Compatibility: broadening the table changes observed output; later changes
  require explicit review.
- Performance: bounded allowlisted traversal and per-file size limits prevent
  whole-repository or unbounded reads.
- Maintenance: classification must remain centralized in the typed rule table.

## Test Plan

- [ ] Build one fixture containing every allowlisted file/directory class.
- [ ] Add missing, unrelated, nested ordering, in-root symlink, escaping
      symlink, cycle, special-file, oversized, and read-failure fixtures.
- [ ] Validate every emitted component with the ASC-001 API.
- [ ] Run `cargo check -p harness-core --all-targets`.
- [ ] Run `cargo test -p harness-core stack::inventory_tests`.
- [ ] Run `cargo test -p harness-core`.
- [ ] Run `cargo fmt --all` and `cargo fmt --all -- --check`.
- [ ] Before push, run
      `cargo clippy --workspace --all-targets -- -D warnings`.
- [ ] Run
      `python3 checks/check_workflow.py --repo . --spec-dir specs/GH1731`.
- [ ] Confirm the implementation diff contains only the three paths in the
      planned-changes manifest.

## Rollback Plan

Revert the implementation commit. This service writes no data and has no
public CLI consumer in this issue. If later consumers have landed, disable or
revert them first, then remove the inventory submodule while retaining the
ASC-001 component model.
