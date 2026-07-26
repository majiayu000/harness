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
- `crates/harness-core/src/stack/mod.rs:10-319` now owns the merged ASC-001
  component contract. This issue constructs repository sources with
  `AgentStackSource::new`, hashes exact opened bytes with
  `Sha256Digest::from_bytes`, and builds validated components with
  `AgentStackComponent::new` plus `with_integrity`.
- `crates/harness-core/Cargo.toml:25-30` already includes sha2 and tempfile but
  has no capability-scoped filesystem API. Bare `std::fs` path validation
  cannot bind containment checks to the file handle opened after the check.

## Proposed Design

### Inventory Service

Add `stack::inventory` with:

- `AgentStackInventoryOptions`;
- `AgentStackInventory`;
- `AgentStackInventoryEntry`;
- `AgentStackEntryClass`;
- `AgentStackInventoryErrorKind`;
- `AgentStackInventoryError`;
- a private `InventoryRule` table;
- `inventory_repository_stack(options) -> Result<AgentStackInventory, ...>`.

`stack/mod.rs` exposes the service and its public option/result/error types. It
does not expose traversal helpers or mutable rule tables.

`AgentStackInventoryOptions` has private fields and a constructor accepting a
`PathBuf` root. Its byte limits are `u64`; its file-count, depth, and
entries-per-directory limits are `usize`. The defaults are 1 MiB per file,
64 MiB total, 10,000 regular files, depth 32, and 10,000 entries per directory.
Builder methods return a validated options value. Every limit is non-zero,
byte limits must permit a checked `+ 1` sentinel, and invalid values fail
before the root is opened. The allowlisted directory itself is depth 0; each
descended directory increments depth by one. `max_files` counts regular files,
while the single non-recursive `spec` directory-presence observation consumes
no file or byte budget.

`AgentStackInventory` has one private ordered entry vector and exposes it as a
slice. It does not expose an ambient or canonical root path:
`cap_std::fs::Dir::canonicalize` returns a capability-relative path and cannot
prove an ambient absolute identity. `AgentStackInventoryEntry` has private
component and entry-class fields with read-only accessors. Regular-file class
stores `unix_executable: Option<bool>`; directory presence has no executable
fact.

`AgentStackInventoryError` contains an `AgentStackInventoryErrorKind` and an
optional sanitized repository-relative locator. The closed initial categories
are `invalid_options`, `root_open`, `entry_metadata`, `broken_symlink`,
`root_escape`, `non_regular_entry`, `non_utf8_locator`, `read_failed`,
`limit_exceeded`, and `component_validation`. Error evidence never serializes
file bytes, ambient target paths, or arbitrary OS error strings. This issue
does not add an aggregate digest or snapshot identity.

### Closed Discovery Table

Define the B-003 allowlist as one constant typed table. Every row has:

- repository-relative matcher (exact path or root-only suffix);
- expected entry class (`file`, recursively scanned `directory`, or
  non-recursive `directory_presence`);
- `AgentStackComponentKind`.

Root instruction names map to `instructions`, workflow files and
`.github/workflows` map to `workflow`, skill roots map to `skill`, hook roots
map to `hook`, MCP files map to `mcp_server`, memory/remem surfaces map to
`memory`, policy/rule surfaces map to `policy`, Harness configuration maps to
`validation`, and package/toolchain files map to `validation`.

Harness-native rows are exact and independently typed:
`.harness/config.toml` maps to `validation`, `.harness/skills` to `skill`,
`.harness/rules` and `.harness/sg` to `policy`, and `.harness/guards` to
`hook`. There is no recursive `.harness` row. Consequently runtime logs/PIDs
under `.harness/local`, GC drafts/checkpoints, and generated adoption artifacts
cannot enter inventory output or consume scan budgets.

The instruction rows include `AGENTS.md`, `AGENTS.override.md`, and
`CLAUDE.md` directly beneath each of `src`, `crates`, `lib`, and `pkg`, matching
the paths that `load_agents_md` may load. These are exact file rows, not
recursive scans of those code directories.

The validation rows exhaustively mirror every repository predicate consumed by
`lang_detect.rs`: exact files `Cargo.toml`, `go.mod`, `package.json`,
`pyproject.toml`, `setup.py`, `requirements.txt`, `build.gradle`,
`build.gradle.kts`, `pom.xml`, `Gemfile`, `yarn.lock`, `pnpm-lock.yaml`,
`.eslintrc`, `.eslintrc.js`, `.eslintrc.cjs`, `.eslintrc.json`,
`.eslintrc.yaml`, `.eslintrc.yml`, `eslint.config.js`, `eslint.config.mjs`,
`eslint.config.cjs`, `biome.json`, and `.rubocop.yml`; root-only suffix rows
for `*.csproj` and `*.sln`; and a `directory_presence` row for root `spec`.
`Cargo.toml` and `package.json` content digests bind the workspace and test-
script predicates respectively. `spec` emits only a presence component and is
not recursively scanned. `Makefile` and `justfile` remain additional exact
toolchain rows.

The table order is stable but final output is sorted by normalized locator, so
adding a rule cannot reorder unrelated existing components.

### Safe Traversal

Run a baseline `cargo audit` before changing either manifest, then add the
audited `cap-std` workspace and `harness-core` dependencies. Call
`Dir::open_ambient_dir` exactly once for the caller-supplied root and treat that
opened directory handle—not a reconstructed ambient path—as the root identity
and access boundary. Do not canonicalize and reopen the root by pathname.
Perform all discovery, metadata, directory iteration, and file opens relative
to the resulting `cap_std::fs::Dir`; no descendant operation converts back to
an ambient path or uses `std::fs` free functions.

Validate every numeric limit before traversal. `max_file_bytes` must permit a
checked `+ 1` sentinel read; zero limits and arithmetic overflow return a typed
configuration error. For each rule:

1. inspect every exact allowlist path with capability-relative,
   non-following `symlink_metadata`;
2. treat `NotFound` only from that initial non-following lookup as absence; an
   entry already yielded by `read_dir` followed by `NotFound` is a race error;
3. follow or open an observed symlink through the directory capability and
   classify `NotFound` as `broken_symlink`; reject escaping targets and every
   other metadata or path-resolution failure;
4. visit directories through capability-relative handles, collect at most
   `max_entries_per_directory + 1` entries with checked arithmetic, fail on the
   sentinel entry, then normalize losslessly and sort before recursion;
5. track opened directory identities in the active ancestor stack to reject
   cycles while permitting non-cyclic duplicate paths to the same directory;
6. increment checked depth and file-count budgets before descending or reading
   and fail before a configured limit can be exceeded;
7. reject sockets, FIFOs, devices, and every other non-regular special entry;
8. open each file through its containing directory capability and validate
   regular-file metadata from the opened handle, which becomes the authority
   for the observed type, bytes, and executable mode;
9. compute checked per-file and remaining-aggregate `+ 1` sentinels, read in
   bounded chunks through `File::take(min(per_file_sentinel,
   remaining_total_sentinel))`, account each chunk before the next read, and
   reject immediately when either byte limit would be exceeded;
10. calculate SHA-256 and Unix executable metadata from the exact opened file
    handle.

Safe in-root symlinks retain their link locator as component identity while
hashing target bytes. If a symlink changes between two valid in-root regular
files, the scan may observe either opened target; it does not claim to detect
that swap. Duplicate target content may therefore produce multiple components
with distinct repository locators, which accurately reflects multiple declared
stack entry points.

Errors carry a stable category and sanitized repository-relative locator.
They do not include file bytes, resolved out-of-root locations, or arbitrary OS
error strings in serialized evidence.

Capability-relative lookup and open bind root containment to directory/file
handles even when a pathname or symlink changes concurrently. The scan is not
an atomic filesystem snapshot: an in-root symlink swap may select either valid
target and an in-place writer can change bytes during a read. The digest remains
the digest of bytes actually returned by the opened handle, and callers that
require repeatable snapshot semantics must scan a quiescent checkout.

### Component Construction

Each regular file becomes an `AgentStackInventoryEntry` containing an ASC-001
component constructed through the merged public API:

- `AgentStackSource::new(AgentStackSourceScope::Repository, locator)`;
- `Sha256Digest::from_bytes(opened_bytes)`;
- `AgentStackComponent::new(rule.kind, source,
  AgentStackObservationClass::RepositoryObserved,
  AgentStackSelectionState::Discovered,
  AgentStackTrustLevel::RepositoryObserved, AgentStackFreshness::Unknown)`;
- `.with_integrity(Some(digest))` for regular files;
- the constructor-derived component ID
  `repository:<component kind>:<normalized locator>`;
- no capabilities.

`AgentStackEntryClass::RegularFile` additionally carries
`unix_executable: Some(bool)` from the opened handle's `mode & 0o111` on Unix,
or `None` on platforms where that metadata is not observed. The
`DirectoryPresence` class is used only for root `spec`; its ASC-001 integrity
is absent, its kind is `validation`, and all other observation fields match a
discovered repository component. Aggregate snapshot and diff consumers must
compare the full entry rather than only the content digest.

Construction calls ASC-001 validation. Any validation failure becomes an
inventory error; the service never skips an invalid component.

### Test Layout

Keep production traversal in `inventory.rs` and table-driven fixtures in
`inventory_tests.rs`. Tests use tempfile and explicit permissions where the
platform supports unreadable-file assertions. Platform-specific inability to
create an unreadable file must use a deterministic injected reader failure
fixture rather than silently skip the behavior. Unix-only non-UTF-8 fixtures
assert a typed locator error; other platforms assert the same validator
directly. A coordinated escaping-symlink fixture proves that a raced path
cannot escape the opened root capability. An in-root swap fixture accepts
either valid opened target and proves the digest matches that handle. A
root-path replacement fixture proves traversal remains bound to the originally
opened handle without claiming an ambient canonical root. Unix fixtures toggle
a hook's executable bits while holding bytes constant; non-Unix tests assert
the explicit unobserved state.

## Data Flow

Explicit root/options → validated limits → one ambient root-handle open → fixed
rule lookup → safe, sorted capability-relative traversal →
remaining-budget-capped byte read → SHA-256 and file metadata → validated
ASC-001 component → typed entry → ordered `AgentStackInventory`.

Any existing-entry failure aborts the operation. Missing allowlisted entries
produce no component. No subprocess, network, persistence, or repository write
occurs.

## Product-to-Test Mapping

| Behavior invariant | Implementation area | Verification |
| --- | --- | --- |
| B-001 | root-handle-first preflight with no ambient-root claim | `cargo test -p harness-core stack::inventory_tests::inventory_stays_bound_to_the_opened_root_handle` |
| B-002 | root-only rule table | `cargo test -p harness-core stack::inventory_tests::inventory_never_reads_user_global_or_sibling_paths` |
| B-003 | `InventoryRule` constant | `cargo test -p harness-core stack::inventory_tests::inventory_discovers_every_stack_and_language_validation_selector` |
| B-004 | non-following initial lookup and missing-entry handling | `cargo test -p harness-core stack::inventory_tests::missing_allowlisted_entries_emit_no_placeholders` |
| B-005 | entry/component construction, hashing, executable mode, and directory presence | `cargo test -p harness-core stack::inventory_tests::entries_bind_content_mode_and_directory_presence_to_valid_components` |
| B-006 | typed rule classification | `cargo test -p harness-core stack::inventory_tests::component_kind_comes_from_matching_rule` |
| B-007 | sorted traversal/output | `cargo test -p harness-core stack::inventory_tests::filesystem_enumeration_order_does_not_change_inventory` |
| B-008 | capability-relative traversal/open and opened-handle observation | `cargo test -p harness-core stack::inventory_tests::symlink_swaps_remain_root_confined_and_hash_the_opened_target` |
| B-009 | typed read/path errors | `cargo test -p harness-core stack::inventory_tests::unreadable_and_non_utf8_entries_fail_without_lossy_locators` |
| B-010 | checked streaming aggregate/per-entry limits | `cargo test -p harness-core stack::inventory_tests::reads_never_exceed_remaining_aggregate_or_per_file_budget` |
| B-011 | repeated full-entry fixture comparison | `cargo test -p harness-core stack::inventory_tests::unchanged_repository_inventory_is_repeatable` |
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
  race symlink replacement against handle-relative opens. Run a baseline
  `cargo audit` before adding the security-sensitive dependency and run it
  again after the manifest and lockfile change.
- Logic: an incomplete allowlist omits behavior-affecting sources. The product
  contract and table test enumerate the exact v0.1 surface, including every
  current `lang_detect.rs` predicate.
- Compatibility: broadening the table changes observed output; later changes
  require explicit review.
- Performance: checked file-count, aggregate-byte, depth,
  entries-per-directory, and per-file limits bound traversal work.
- Maintenance: classification must remain centralized in the typed rule table.

## Test Plan

- [ ] Build one fixture containing every allowlisted file/directory class.
- [ ] Assert exact coverage of every language, package-manager, linter, and
      test-directory selector consumed by `lang_detect.rs`.
- [ ] Add missing, unrelated, nested ordering, in-root symlink, escaping
      symlink, root-swap, symlink-race, cycle, special-file, non-UTF-8,
      per-file, remaining-aggregate-byte, file-count, depth,
      entries-per-directory, overflow, and read-failure fixtures.
- [ ] Add Unix executable-bit change and non-Unix unobserved-mode fixtures, plus
      a non-recursive root `spec` directory-presence fixture.
- [ ] Validate every emitted component with the ASC-001 API.
- [ ] Before editing `Cargo.toml`, `crates/harness-core/Cargo.toml`, or
      `Cargo.lock`, run baseline `cargo audit`.
- [ ] Run `cargo check -p harness-core --all-targets`.
- [ ] Run `cargo test -p harness-core stack::inventory_tests`.
- [ ] Run `cargo test -p harness-core`.
- [ ] Run `cargo fmt --all` and `cargo fmt --all -- --check`.
- [ ] Before push, run
      `cargo clippy --workspace --all-targets -- -D warnings`.
- [ ] Run `cargo audit` again after adding `cap-std` and updating `Cargo.lock`.
- [ ] Run
      `python3 checks/check_workflow.py --repo . --spec-dir specs/GH1731`.
- [ ] Confirm the implementation diff contains only the six paths in the
      planned-changes manifest.

## Rollback Plan

Revert the implementation commit. This service writes no data and has no
public CLI consumer in this issue. If later consumers have landed, disable or
revert them first, then remove the inventory submodule while retaining the
ASC-001 component model.
