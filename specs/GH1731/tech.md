# Tech Spec

## Linked Issue

GH-1731

## Product Spec

See `specs/GH1731/product.md`.

<!-- specrail-planned-changes
{"issue":1731,"complete":true,"paths":["Cargo.lock","Cargo.toml","crates/harness-core/Cargo.toml","crates/harness-core/src/stack/inventory.rs","crates/harness-core/src/stack/inventory_tests.rs","crates/harness-core/src/stack/mod.rs"],"spec_refs":["B-001","B-002","B-003","B-004","B-005","B-006","B-007","B-008","B-009","B-010","B-011","B-012"]}
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
- `crates/harness-core/Cargo.toml:25-30` already includes sha2 and tempfile but
  has no capability-scoped filesystem API. Bare `std::fs` path validation
  cannot bind containment checks to the file handle opened after the check.

## Proposed Design

### Inventory Service

Add `stack::inventory` with:

- `AgentStackInventoryOptions { root, max_file_bytes, max_total_bytes,
  max_files, max_depth, max_entries_per_directory }`;
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

The instruction rows include `AGENTS.md`, `AGENTS.override.md`, and
`CLAUDE.md` directly beneath each of `src`, `crates`, `lib`, and `pkg`, matching
the paths that `load_agents_md` may load. These are exact file rows, not
recursive scans of those code directories.

The table order is stable but final output is sorted by normalized locator, so
adding a rule cannot reorder unrelated existing components.

### Safe Traversal

Add audited `cap-std` workspace and `harness-core` dependencies. Open the
explicit root once with ambient authority, then perform all discovery,
metadata, directory iteration, and file opens relative to that
`cap_std::fs::Dir`. No descendant operation converts back to an ambient path or
uses `std::fs` free functions.

Validate every numeric limit before traversal. `max_file_bytes` must permit a
checked `+ 1` sentinel read; zero limits and arithmetic overflow return a typed
configuration error. For each rule:

1. inspect relative metadata through the root directory capability;
2. treat `NotFound` from that initial lookup as absence;
3. return a typed error for every other metadata or path-resolution failure,
   including broken and escaping symlinks;
4. visit directories through capability-relative handles, collect at most
   `max_entries_per_directory` entries, normalize losslessly, and sort before
   recursion;
5. track opened directory identities in the active ancestor stack to reject
   cycles while permitting non-cyclic duplicate paths to the same directory;
6. increment checked depth, file-count, and aggregate-byte budgets before
   descending or reading and fail before a configured limit can be exceeded;
7. reject sockets, FIFOs, devices, and every other non-regular special entry;
8. open each file through its containing directory capability and validate
   regular-file metadata from the opened handle;
9. read through `File::take(checked_max_file_bytes_plus_one)` and reject
   per-file or aggregate overflow;
10. calculate SHA-256 from the exact bytes returned by that opened handle.

Safe in-root symlinks retain their link locator as component identity while
hashing target bytes. Duplicate target content may therefore produce multiple
components with distinct repository locators, which accurately reflects
multiple declared stack entry points.

Errors carry a stable category and sanitized repository-relative locator.
They do not include file bytes, resolved out-of-root locations, or arbitrary OS
error strings in serialized evidence.

Capability-relative lookup and open bind root containment to directory/file
handles even when a pathname or symlink changes concurrently. The scan is not
an atomic filesystem snapshot: an in-place writer can change bytes during a
read. The digest remains the digest of bytes actually returned by the opened
handle, and callers that require repeatable snapshot semantics must scan a
quiescent checkout.

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
fixture rather than silently skip the behavior. Unix-only non-UTF-8 fixtures
assert a typed locator error; other platforms assert the same validator
directly. A coordinated symlink-swap fixture proves that a raced path cannot
escape the opened root capability.

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
| B-003 | `InventoryRule` constant | `cargo test -p harness-core stack::inventory_tests::inventory_discovers_every_v0_1_allowlisted_surface_including_loaded_subdirectory_instructions` |
| B-004 | NotFound handling | `cargo test -p harness-core stack::inventory_tests::missing_allowlisted_entries_emit_no_placeholders` |
| B-005 | component construction and hashing | `cargo test -p harness-core stack::inventory_tests::discovered_files_emit_valid_repository_observed_components` |
| B-006 | typed rule classification | `cargo test -p harness-core stack::inventory_tests::component_kind_comes_from_matching_rule` |
| B-007 | sorted traversal/output | `cargo test -p harness-core stack::inventory_tests::filesystem_enumeration_order_does_not_change_inventory` |
| B-008 | capability-relative traversal/open | `cargo test -p harness-core stack::inventory_tests::raced_or_escaping_symlinks_cannot_escape_root_capability` |
| B-009 | typed read/path errors | `cargo test -p harness-core stack::inventory_tests::unreadable_and_non_utf8_entries_fail_without_lossy_locators` |
| B-010 | checked aggregate/per-entry limits | `cargo test -p harness-core stack::inventory_tests::inventory_rejects_special_entries_and_every_resource_limit` |
| B-011 | repeated fixture comparison | `cargo test -p harness-core stack::inventory_tests::unchanged_repository_inventory_is_repeatable` |
| B-012 | read-only API surface and side-effect fixture | `cargo test -p harness-core stack::inventory_tests::inventory_is_read_only_and_invokes_no_external_behavior`; `rg -n "Command::new|TcpStream|UdpSocket|reqwest|std::fs::(write|remove|rename|create_dir)" crates/harness-core/src/stack/inventory.rs` (expect no matches); `cargo test -p harness-core agents_md` |

## Alternatives Considered

- Extend `agents_md::discovery_paths`: rejected because it intentionally
  includes global configuration and only instruction files.
- Drive discovery from `SkillStore`: rejected because non-skill stack
  components and repository-only trust do not belong to skill governance.
- Recursively scan the complete repository: rejected for performance, privacy,
  generated-file noise, and false claims about behavioral relevance.
- Use `walkdir` or bare `std::fs`: rejected because neither binds containment
  checks to descendant opens across symlink/path races.
- Continue after unreadable files: rejected because a partial inventory would
  look complete and violate fail-closed evidence semantics.

## Risks

- Security: path escape and symlink races could read outside the requested
  root. All descendant operations use the root directory capability, and tests
  race symlink replacement against handle-relative opens. Run `cargo audit`
  when introducing the security-sensitive dependency.
- Logic: an incomplete allowlist omits behavior-affecting sources. The product
  contract and table test enumerate the exact v0.1 surface.
- Compatibility: broadening the table changes observed output; later changes
  require explicit review.
- Performance: checked file-count, aggregate-byte, depth,
  entries-per-directory, and per-file limits bound traversal work.
- Maintenance: classification must remain centralized in the typed rule table.

## Test Plan

- [ ] Build one fixture containing every allowlisted file/directory class.
- [ ] Add missing, unrelated, nested ordering, in-root symlink, escaping
      symlink, symlink-race, cycle, special-file, non-UTF-8, per-file,
      aggregate-byte, file-count, depth, entries-per-directory, overflow, and
      read-failure fixtures.
- [ ] Validate every emitted component with the ASC-001 API.
- [ ] Run `cargo check -p harness-core --all-targets`.
- [ ] Run `cargo test -p harness-core stack::inventory_tests`.
- [ ] Run `cargo test -p harness-core`.
- [ ] Run `cargo fmt --all` and `cargo fmt --all -- --check`.
- [ ] Before push, run
      `cargo clippy --workspace --all-targets -- -D warnings`.
- [ ] Run `cargo audit` after adding `cap-std`.
- [ ] Run
      `python3 checks/check_workflow.py --repo . --spec-dir specs/GH1731`.
- [ ] Confirm the implementation diff contains only the six paths in the
      planned-changes manifest.

## Rollback Plan

Revert the implementation commit. This service writes no data and has no
public CLI consumer in this issue. If later consumers have landed, disable or
revert them first, then remove the inventory submodule while retaining the
ASC-001 component model.
