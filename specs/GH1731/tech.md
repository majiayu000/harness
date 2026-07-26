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
`PathBuf` root. Its byte limits are `u64`; its regular-file, opened-directory,
aggregate-encountered-entry, depth, and entries-per-directory limits are
`usize`. The defaults are 1 MiB per file, 64 MiB total, 10,000 regular files,
1,000 opened directories, 50,000 aggregate encountered entries, depth 32, and
10,000 entries per directory. Builder methods return a validated options
value. Every limit is non-zero, byte and entry limits must permit a checked
`+ 1` sentinel, and invalid values fail before the root is opened. The opened
repository root counts as directory 1 and is depth 0; each recursively opened
directory increments the directory budget and depth. Every item yielded by
`read_dir` increments `max_total_entries` before classification, including
entries later rejected or excluded. `max_files` counts regular files, while the
single non-recursive `spec` directory-presence observation consumes no file,
directory, encountered-entry, or byte budget.

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
`entry_raced`, `cycle_detected`, `config_parse`,
`configured_source_invalid`, `configured_source_missing`, `limit_exceeded`,
and `component_validation`. `entry_raced` means an entry already returned by
`read_dir` disappeared or could no longer be opened/represented under the
observed locator; a symlink swapped to another valid in-root regular file is
not an error and follows B-008. `cycle_detected` is used only when an opened
directory identity is already present in the active ancestor stack.
`config_parse` covers invalid UTF-8 or TOML in present `harness.toml`;
`configured_source_invalid` covers an empty, parent-traversing, or otherwise
invalid repository-relative rule source, and `configured_source_missing` means
that such a declared relative source is absent. Error evidence never serializes
file bytes, ambient target paths, configured absolute paths, or arbitrary OS
error strings. This issue does not add an aggregate digest or snapshot identity.

### Closed Discovery Table

Define the B-003 allowlist as one constant typed table. Every row has:

- repository-relative matcher (exact path or root-only suffix);
- expected entry class (`file`, selector-filtered recursive `directory`,
  configured `file_or_directory`, or non-recursive `directory_presence`);
- a closed entry selector for directory rows;
- `AgentStackComponentKind`.

The private directory selector vocabulary is:

- `direct_extension` for definitions registered only as direct children;
- `direct_basename` for one exact registered child name;
- `recursive_extension` for recursive extension-governed sources;
- `recursive_basename` for package entrypoints with one exact basename.

Selectors are data in the typed rule table, not content sniffing or ad hoc
conditionals in traversal. The four general skill roots `.claude/skills`,
`.codex/skills`, `.agents/skills`, and `skills` use the union of direct
extension `md` and recursive basename `SKILL.md`. `.harness/skills` uses only
direct extension `md`, matching `SkillStore::load_from_dir`; in particular,
`*.usage.json`, nested package references, and arbitrary support files do not
emit components. `.github/workflows` uses direct extensions `yml` and `yaml`;
`.harness/rules` and `rules` use recursive extensions `md` and `toml`;
`.harness/guards` uses direct extension `sh`; `.harness/sg` uses recursive
extensions `yml` and `yaml`; and `.cursor/rules` uses recursive extensions
`md` and `mdc`. `.vibeguard` uses recursive extensions `md`, `toml`, `yaml`,
`yml`, `json`, and `json5`; `.remem` uses recursive extensions `toml`, `yaml`,
`yml`, and `json`, so databases and runtime state are excluded. The exact
`.vibeguard/run-guards.sh` row is independently typed `validation`; other shell
helpers beneath `.vibeguard` are not selected.

`.githooks` uses direct basenames from this closed Git lifecycle vocabulary:
`applypatch-msg`, `pre-applypatch`, `post-applypatch`, `pre-commit`,
`pre-merge-commit`, `prepare-commit-msg`, `commit-msg`, `post-commit`,
`pre-rebase`, `post-checkout`, `post-merge`, `pre-push`, `pre-receive`,
`update`, `proc-receive`, `post-receive`, `post-update`,
`reference-transaction`, `push-to-checkout`, `pre-auto-gc`, `post-rewrite`,
`sendemail-validate`, `fsmonitor-watchman`, `p4-changelist`,
`p4-prepare-changelist`, `p4-post-changelist`, `p4-pre-submit`, and
`post-index-change`. Generic `.claude/hooks` and `hooks` rows do not exist:
without an explicit lifecycle binding, their README, helpers, and fixtures
cannot satisfy the ASC-001 `hook` kind.

If root `harness.toml` is present, parse its already-bounded opened bytes into a
private minimal config shape containing only `rules.discovery_paths`,
`rules.builtin_path`, `rules.exec_policy_paths`, and
`rules.requirements_path`. Do not deserialize unrelated Harness settings or
reopen the config. Normalize each non-empty relative path lexically, reject any
parent traversal, and derive a typed `policy` rule:

- `discovery_paths` and `builtin_path` accept either one exact file or a
  directory recursively selecting extensions `md` and `toml`;
- `exec_policy_paths` and `requirements_path` require one exact file;
- duplicate `(normalized locator, component kind)` bindings are inventoried
  once; a locator selected under different kinds emits one component per kind;
- an absolute configured source is outside B-002 and produces no target
  component or serialized path;
- an invalid, escaping, or missing relative source fails typed.

Derived rules use the same capability root, path normalization, selectors,
symlink handling, resource budgets, ordering, and error redaction as static
rules. The `harness.toml` bytes count and hash once even though they also produce
derived rules.

Root instruction names map to `instructions`, workflow files and
`.github/workflows` map to `workflow`, skill roots map to `skill`, selected hook
entrypoints map to `hook`, MCP files map to `mcp_server`, memory/remem surfaces
map to `memory`, policy/rule surfaces map to `policy`, Harness configuration
maps to `validation`, the exact `.vibeguard/run-guards.sh` maps to `validation`,
and package/toolchain files map to `validation`.

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
audited `cap-std` workspace and `harness-core` dependencies. Promote the
already-locked `libc` crate to a workspace dependency used by `harness-core`
only for Unix `O_NONBLOCK`; do not add another syscall wrapper. Call
`Dir::open_ambient_dir` exactly once for the caller-supplied root and treat that
opened directory handle—not a reconstructed ambient path—as the root identity
and access boundary. Do not canonicalize and reopen the root by pathname.
Perform all discovery, metadata, directory iteration, and file opens relative
to the resulting `cap_std::fs::Dir`; no descendant operation converts back to
an ambient path or uses `std::fs` free functions.

Validate every numeric limit before traversal. `max_file_bytes` must permit a
checked `+ 1` sentinel read; zero limits and arithmetic overflow return a typed
configuration error. Then read and emit `harness.toml` at most once through the
same bounded handle-first path, parse only the rule-source fields above, and
merge their derived rules with the static table before traversing other entries.
For each expanded rule:

1. inspect every exact static or derived path with capability-relative,
   non-following `symlink_metadata`;
2. treat `NotFound` from an initial static optional lookup as absence, but map it
   to `configured_source_missing` for a repository-relative derived rule. An
   entry already yielded by `read_dir` followed by `NotFound` is a race error;
3. visit directories through capability-relative handles, collect at most
   `max_entries_per_directory + 1` entries with checked arithmetic, fail on the
   sentinel entry, and charge every yielded entry to `max_total_entries` before
   classification;
4. inspect the yielded entry's non-following type. For an ordinary non-directory
   entry, compare ASCII basename and extension selectors against its native
   `OsStr` before portable normalization; exclude an unmatched entry without
   opening it or requiring UTF-8. A directory required for recursive discovery
   must have a lossless portable locator before descent;
5. for a symlink, resolve target metadata through the directory capability
   before applying a file selector. Classify `NotFound` as `broken_symlink`,
   reject escape or other resolution failure, and descend through an in-root
   directory target only when the rule expects `directory` or
   `file_or_directory`. A `file` rule whose symlink target is a directory
   returns `non_regular_entry`. Apply the native basename/extension selector
   only when the resolved target is not a directory;
6. normalize every selected file and recursively visited directory losslessly,
   sort those candidates by portable locator, and then process them. A non-UTF-8
   unmatched ordinary file is ignored, while a non-UTF-8 selected file or
   directory required for recursion returns `non_utf8_locator`;
7. track opened directory identities in the active ancestor stack to reject
   cycles as `cycle_detected` while permitting non-cyclic duplicate paths to
   the same directory;
8. increment checked depth, opened-directory, and file-count budgets before
   descending or reading and fail before a configured limit can be exceeded;
9. open every selected file through its containing directory capability. On
   Unix, construct `cap_std::fs::OpenOptions` with read access and
   `libc::O_NONBLOCK` through
   `cap_std::fs::OpenOptionsExt::custom_flags`; on other platforms use read-only
   handle open. Validate regular-file metadata from the opened handle before the
   first content read. A raced FIFO, socket, device, directory, or other
   non-regular opened handle returns `non_regular_entry` and cannot block the
   inventory worker;
10. compute checked per-file and remaining-aggregate `+ 1` sentinels, read in
    bounded chunks through `File::take(min(per_file_sentinel,
    remaining_total_sentinel))`, account each chunk before the next read, and
    reject immediately when either byte limit would be exceeded;
11. calculate SHA-256 and Unix executable metadata from the exact opened file
    handle.

Safe in-root symlinks retain their link locator as component identity while
hashing target bytes. If a symlink changes between two valid in-root regular
files, the scan may observe either opened target; it does not claim to detect
that swap and does not emit `entry_raced`. If an entry already yielded by
`read_dir` disappears or cannot be opened under its observed locator, the scan
returns `entry_raced`. Duplicate target content may therefore produce multiple
components with distinct repository locators, which accurately reflects
multiple declared stack entry points.

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
  AgentStackTrustLevel::RepositoryObserved,
  AgentStackFreshnessEvidence::new(false, None, None, true, false).classify())`;
- `.with_integrity(Some(digest))` for regular files;
- the constructor-derived component ID
  `repository:<component kind>:<normalized locator>`;
- no capabilities.

`AgentStackEntryClass::RegularFile` additionally carries
`unix_executable: Some(bool)` from the opened handle's `mode & 0o111` on Unix,
or `None` on platforms where that metadata is not observed. The
`DirectoryPresence` class is used only for root `spec`; its ASC-001 integrity
is absent, its kind is `validation`, and all other observation fields match a
discovered repository component. Both regular-file reads and the successful
`spec` directory probe classify as `fresh`; `unknown` is not valid for a
successful current inventory observation. Aggregate snapshot and diff consumers
must compare the full entry rather than only the content digest.

Construction calls ASC-001 validation. Any validation failure becomes an
inventory error; the service never skips an invalid component.

### Test Layout

Keep production traversal in `inventory.rs` and table-driven fixtures in
`inventory_tests.rs`. Tests use tempfile and explicit permissions where the
platform supports unreadable-file assertions. Platform-specific inability to
create an unreadable file must use a deterministic injected reader failure
fixture rather than silently skip the behavior. Unix-only non-UTF-8 fixtures
assert that unmatched native names are excluded before normalization and that a
selected name or traversed directory returns a typed locator error; other
platforms assert the same validator directly. Exact hook fixtures include valid
lifecycle basenames plus README, helpers, and nested fixtures. A Unix FIFO
fixture uses a bounded test timeout and proves `O_NONBLOCK` reaches handle-type
rejection without a writer. A coordinated escaping-symlink fixture proves that
a raced path cannot escape the opened root capability. An in-root swap fixture
accepts either valid opened target and proves the digest matches that handle. A
directory-symlink fixture proves recursive discovery and cycle rejection. A
root-path replacement fixture proves traversal remains bound to the originally
opened handle without claiming an ambient canonical root. Unix fixtures toggle
a hook's executable bits while holding bytes constant; non-Unix tests assert
the explicit unobserved state.

## Data Flow

Explicit root/options → validated limits → one ambient root-handle open →
bounded `harness.toml` read and configured-rule derivation → static/derived rule
merge → native selector filtering → safe, sorted capability-relative traversal
→ nonblocking candidate open and handle-type validation →
remaining-budget-capped byte read → SHA-256 and file metadata → validated
ASC-001 component → typed entry → ordered `AgentStackInventory`.

Any selected-entry or required traversal-directory failure aborts the operation.
Missing allowlisted entries and unmatched descendants produce no component. No
subprocess, network, persistence, or repository write occurs.

## Product-to-Test Mapping

| Behavior invariant | Implementation area | Verification |
| --- | --- | --- |
| B-001 | root-handle-first preflight with no ambient-root claim | `cargo test -p harness-core stack::inventory_tests::inventory_stays_bound_to_the_opened_root_handle` |
| B-002 | root-only rule table | `cargo test -p harness-core stack::inventory_tests::inventory_never_reads_user_global_or_sibling_paths` |
| B-003 | static `InventoryRule` table plus bounded configured rule derivation | `cargo test -p harness-core stack::inventory_tests::inventory_discovers_every_stack_and_language_validation_selector`; `cargo test -p harness-core stack::inventory_tests::vibeguard_runner_is_inventoried_as_validation`; `cargo test -p harness-core stack::inventory_tests::configured_repository_rule_sources_are_inventoried_once`; `cargo test -p harness-core stack::inventory_tests::same_locator_with_distinct_kinds_is_preserved` |
| B-004 | non-following initial lookup and missing-entry handling | `cargo test -p harness-core stack::inventory_tests::missing_allowlisted_entries_emit_no_placeholders` |
| B-005 | entry/component construction, hashing, executable mode, directory presence, and current-observation freshness | `cargo test -p harness-core stack::inventory_tests::entries_bind_content_mode_and_directory_presence_to_valid_components`; `cargo test -p harness-core stack::inventory_tests::current_observations_are_fresh` |
| B-006 | surface-specific static and configured selector classification | `cargo test -p harness-core stack::inventory_tests::sidecars_and_support_files_do_not_emit_stack_units`; `cargo test -p harness-core stack::inventory_tests::only_lifecycle_bound_hook_entrypoints_are_inventoried`; `cargo test -p harness-core stack::inventory_tests::configured_repository_rule_sources_are_inventoried_once`; `cargo test -p harness-core stack::inventory_tests::component_kind_comes_from_matching_rule` |
| B-007 | native prefilter plus sorted selected traversal/output | `cargo test -p harness-core stack::inventory_tests::unsupported_non_utf8_entries_are_filtered_before_locator_normalization`; `cargo test -p harness-core stack::inventory_tests::filesystem_enumeration_order_does_not_change_inventory` |
| B-008 | capability-relative file/directory symlink traversal and opened-handle observation | `cargo test -p harness-core stack::inventory_tests::symlink_swaps_remain_root_confined_and_hash_the_opened_target`; `cargo test -p harness-core stack::inventory_tests::in_root_directory_symlinks_are_traversed_and_cycles_fail`; `cargo test -p harness-core stack::inventory_tests::file_rules_reject_directory_symlink_targets` |
| B-009 | typed read/path/config errors | `cargo test -p harness-core stack::inventory_tests::unreadable_and_non_utf8_entries_fail_without_lossy_locators`; `cargo test -p harness-core stack::inventory_tests::invalid_configured_rule_sources_fail_typed` |
| B-010 | checked file/directory/entry/depth/byte limits and nonblocking handle validation | `cargo test -p harness-core stack::inventory_tests::every_traversal_limit_has_an_exact_boundary_fixture`; `cargo test -p harness-core stack::inventory_tests::selected_fifo_targets_fail_without_blocking` |
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
- Performance: checked file-count, opened-directory, aggregate-entry,
  aggregate-byte, depth, entries-per-directory, and per-file limits bound
  traversal work; Unix candidate opens are nonblocking.
- Maintenance: classification must remain centralized in the typed rule table.

## Test Plan

- [ ] Build one fixture containing every allowlisted file/directory class.
- [ ] Assert exact coverage of every language, package-manager, linter, and
      test-directory selector consumed by `lang_detect.rs`.
- [ ] Add missing, unrelated, nested ordering, in-root symlink, escaping
      symlink, root-swap, symlink-race, cycle, special-file, non-UTF-8,
      per-file, remaining-aggregate-byte, file-count, opened-directory,
      aggregate-encountered-entry, depth, entries-per-directory, overflow, and
      read-failure fixtures.
- [ ] Prove direct Markdown and nested `SKILL.md` skill selectors include only
      registered definition entrypoints, while `.usage.json`, package
      references, and unrelated support files emit no component.
- [ ] Prove closed hook selectors exclude README, helpers, and nested fixtures.
- [ ] Prove `.vibeguard/run-guards.sh` is a `validation` entry and unrelated
      `.vibeguard` shell helpers remain excluded.
- [ ] Prove configured repository-relative rule files and directories are
      inventoried once per `(locator, kind)` binding; distinct typed roles for
      one locator are preserved; invalid, escaping, and missing relative sources
      fail typed; absolute sources remain out of scope and are never serialized.
- [ ] Prove unmatched non-UTF-8 ordinary files are excluded before locator
      normalization, while selected files and traversed directories fail typed.
- [ ] Prove in-root directory symlinks recurse, escaping targets fail, cycles
      fail, file rules reject directory targets, and selected Unix FIFO targets
      return without blocking.
- [ ] Prove every successful file read and `spec` directory probe classifies
      freshness as `fresh`.
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
- [ ] Run `cargo audit` again after adding `cap-std`, promoting `libc`, and
      updating `Cargo.lock`.
- [ ] Run
      `python3 checks/check_workflow.py --repo . --spec-dir specs/GH1731`.
- [ ] Confirm the implementation diff contains only the six paths in the
      planned-changes manifest.

## Rollback Plan

Revert the implementation commit. This service writes no data and has no
public CLI consumer in this issue. If later consumers have landed, disable or
revert them first, then remove the inventory submodule while retaining the
ASC-001 component model.
