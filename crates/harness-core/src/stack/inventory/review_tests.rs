//! White-box remediation regressions for GH-1731 (findings §1-§6).
//!
//! These fixtures exercise the private traversal/merge/read boundaries through
//! real filesystem state and deterministic test seams. They are deliberately
//! white-box: they reach into `super` internals (`merge_derived_rules`,
//! `reopen_raced_directory`, `normalize_configured_source`) and arm the
//! cfg(test) seams that simulate directory-open races, reversed enumeration,
//! and bounded-read failures.

use super::*;
use std::fs;
use std::path::Path;
use tempfile::TempDir;
use AgentStackInventoryErrorKind as EK;

#[cfg(unix)]
use std::os::unix::fs::symlink;

fn tmp() -> TempDir {
    tempfile::tempdir().expect("tempdir")
}

fn write_file(root: &Path, rel: &str, contents: &[u8]) {
    let path = root.join(rel);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("mkdir");
    }
    fs::write(path, contents).expect("write");
}

fn opts(root: &Path) -> AgentStackInventoryOptions {
    AgentStackInventoryOptions::new(root.to_path_buf())
}

fn run(root: &Path) -> Result<AgentStackInventory, AgentStackInventoryError> {
    inventory_repository_stack(&opts(root))
}

fn pairs(inventory: &AgentStackInventory) -> Vec<(String, &'static str)> {
    inventory
        .entries()
        .iter()
        .map(|entry| {
            let component = entry.component();
            (
                component.source().locator().as_str().to_owned(),
                component.kind().as_str(),
            )
        })
        .collect()
}

/// Run `f` with every freshly collected native listing reversed (§5 seam).
fn reversed_listing<R>(f: impl FnOnce() -> R) -> R {
    REVERSE_LISTING.set(true);
    let result = f();
    REVERSE_LISTING.set(false);
    result
}

/// Arm the §3 seam so the initial recursive `open_dir` for `prefix` fails once.
#[cfg(unix)]
fn force_dir_open_not_found(prefix: &str) {
    FORCE_DIR_OPEN_NOT_FOUND.with(|forced| *forced.borrow_mut() = Some(prefix.to_owned()));
}

#[cfg(unix)]
fn dir_open_seam_fired() -> bool {
    // The seam clears itself when it fires; a still-armed seam means the
    // targeted open never happened and the test would pass vacuously.
    FORCE_DIR_OPEN_NOT_FOUND.with(|forced| forced.borrow().is_none())
}

// ---------------------------------------------------------------------------
// §1 — Preserve the strictest derived constraint.
// ---------------------------------------------------------------------------

#[test]
fn derived_exact_file_constraint_wins_over_flexible_source() {
    // A locator listed in both a flexible source (`discovery_paths`) and an
    // exact-file source (`exec_policy_paths`) must reject a directory.
    let dir = tmp();
    let root = dir.path();
    write_file(
        root,
        "harness.toml",
        b"[rules]\ndiscovery_paths = [\"p\"]\nexec_policy_paths = [\"p\"]\n",
    );
    fs::create_dir(root.join("p")).expect("mkdir p");
    let error = run(root).expect_err("exact-file constraint must reject the directory");
    assert_eq!(error.kind(), EK::NonRegularEntry);
    assert_eq!(error.locator(), Some("p"));
}

#[test]
fn exact_configured_source_tightens_static_recursive_rule() {
    // `requirements_path = "rules"` overlaps the static recursive rule
    // `dr("rules", MD_TOML)` and must tighten it to an exact file, so the
    // existing directory is rejected instead of traversed.
    let dir = tmp();
    let root = dir.path();
    write_file(
        root,
        "harness.toml",
        b"[rules]\nrequirements_path = \"rules\"\n",
    );
    write_file(root, "rules/p.md", b"policy");
    let error = run(root).expect_err("static recursive rule must be tightened to File");
    assert_eq!(error.kind(), EK::NonRegularEntry);
    assert_eq!(error.locator(), Some("rules"));
}

#[test]
fn derived_rule_merge_is_field_order_independent() {
    // White-box: composing the strictest constraint per (locator, kind) is
    // independent of the order configured fields feed the merge.
    let build = |derived: Vec<DerivedRule>| {
        let mut exact_rules: Vec<ExactRule> = vec![(
            "p".to_owned(),
            RuleTarget::Directory(MD_TOML),
            Kind::Policy,
            false,
        )];
        merge_derived_rules(&mut exact_rules, derived);
        exact_rules
    };
    let flexible_first = build(vec![
        (
            "p".to_owned(),
            RuleTarget::FileOrDirectory(MD_TOML),
            Kind::Policy,
        ),
        ("p".to_owned(), RuleTarget::File, Kind::Policy),
    ]);
    let exact_first = build(vec![
        ("p".to_owned(), RuleTarget::File, Kind::Policy),
        (
            "p".to_owned(),
            RuleTarget::FileOrDirectory(MD_TOML),
            Kind::Policy,
        ),
    ]);
    assert_eq!(flexible_first.len(), 1, "equivalent bindings traverse once");
    assert_eq!(exact_first.len(), 1, "equivalent bindings traverse once");
    assert!(
        matches!(flexible_first[0].1, RuleTarget::File),
        "exact-file wins when the flexible source is merged first"
    );
    assert!(
        matches!(exact_first[0].1, RuleTarget::File),
        "exact-file wins when the exact source is merged first"
    );
    assert!(
        flexible_first[0].3 && exact_first[0].3,
        "configured overlap is required"
    );

    // When no exact-file constraint exists, the static directory target keeps
    // its closed selector rather than the configured md/toml selector.
    let only_flexible = build(vec![(
        "p".to_owned(),
        RuleTarget::FileOrDirectory(MD_TOML),
        Kind::Policy,
    )]);
    assert!(
        matches!(only_flexible[0].1, RuleTarget::Directory(_)),
        "static directory keeps its closed selector without an exact-file binding"
    );

    // End-to-end: the flexible source cannot erase the later exact-file
    // requirement; the directory is still rejected.
    let dir = tmp();
    let root = dir.path();
    write_file(
        root,
        "harness.toml",
        b"[rules]\ndiscovery_paths = [\"p\"]\nexec_policy_paths = [\"p\"]\n",
    );
    fs::create_dir(root.join("p")).expect("mkdir p");
    let error = run(root).expect_err("directory rejected");
    assert_eq!(error.kind(), EK::NonRegularEntry);
    assert_eq!(error.locator(), Some("p"));
}

// ---------------------------------------------------------------------------
// §4 — Reject Windows drive-relative sources.
// ---------------------------------------------------------------------------

#[test]
fn drive_relative_configured_source_fails_typed() {
    // Lexical classification is host-independent: a drive prefix is absolute
    // only with a root separator after the colon.
    assert_eq!(
        normalize_configured_source("C:policy.toml"),
        Err(EK::ConfiguredSourceInvalid)
    );
    assert_eq!(
        normalize_configured_source("d:rules/p.md"),
        Err(EK::ConfiguredSourceInvalid)
    );
    assert_eq!(
        normalize_configured_source("Z:x"),
        Err(EK::ConfiguredSourceInvalid)
    );
    // Truly absolute sources stay out of repository scope.
    assert_eq!(normalize_configured_source("C:\\policy.toml"), Ok(None));
    assert_eq!(normalize_configured_source("C:/policy.toml"), Ok(None));
    assert_eq!(normalize_configured_source("/abs/p.md"), Ok(None));
    assert_eq!(normalize_configured_source("\\abs\\p"), Ok(None));
    // Portable relative paths still normalize losslessly.
    assert_eq!(
        normalize_configured_source("./rules/p.md"),
        Ok(Some("rules/p.md".to_owned()))
    );
    // End-to-end: a drive-relative configured source fails typed with the
    // config locator on this (non-Windows) host.
    let dir = tmp();
    write_file(
        dir.path(),
        "harness.toml",
        b"[rules]\ndiscovery_paths = [\"C:policy.toml\"]\n",
    );
    let error = run(dir.path()).expect_err("drive-relative source must fail typed");
    assert_eq!(error.kind(), EK::ConfiguredSourceInvalid);
    assert_eq!(error.locator(), Some("harness.toml"));
}

// ---------------------------------------------------------------------------
// §2 — Measure depth from the repository root.
// ---------------------------------------------------------------------------

#[test]
fn nested_allowlist_root_preserves_repository_relative_depth() {
    // `.harness/rules` is physical depth 2; with max_depth 1 it must fail
    // before processing children, and with max_depth 2 it traverses.
    let dir = tmp();
    let root = dir.path();
    write_file(root, ".harness/rules/p.md", b"policy");
    let shallow = opts(root).with_max_depth(1).expect("valid depth");
    let error = inventory_repository_stack(&shallow).expect_err("depth-2 root exceeds max_depth 1");
    assert_eq!(error.kind(), EK::LimitExceeded);
    assert_eq!(error.locator(), Some(".harness/rules"));
    let deep = opts(root).with_max_depth(2).expect("valid depth");
    let inventory = inventory_repository_stack(&deep).expect("depth-2 root fits max_depth 2");
    assert!(pairs(&inventory).contains(&(".harness/rules/p.md".to_owned(), "policy")));
}

#[test]
fn configured_directory_preserves_repository_relative_depth() {
    // A configured directory `x/y` starts traversal at depth 2, not 1.
    let dir = tmp();
    let root = dir.path();
    write_file(
        root,
        "harness.toml",
        b"[rules]\ndiscovery_paths = [\"x/y\"]\n",
    );
    write_file(root, "x/y/p.md", b"policy");
    let shallow = opts(root).with_max_depth(1).expect("valid depth");
    let error =
        inventory_repository_stack(&shallow).expect_err("configured depth-2 exceeds max_depth 1");
    assert_eq!(error.kind(), EK::LimitExceeded);
    assert_eq!(error.locator(), Some("x/y"));
    let deep = opts(root).with_max_depth(2).expect("valid depth");
    let inventory = inventory_repository_stack(&deep).expect("configured depth-2 fits max_depth 2");
    assert!(pairs(&inventory).contains(&("x/y/p.md".to_owned(), "policy")));
}

// ---------------------------------------------------------------------------
// §3 — Reclassify recursive symlink open races.
// ---------------------------------------------------------------------------

#[cfg(unix)]
#[test]
fn recursive_symlink_open_failure_accepts_valid_replacement() {
    // The candidate resolves as a directory, the initial open is raced to
    // NotFound, and the still-present symlink reopens to a valid in-root
    // directory: traversal continues without a second directory/depth charge.
    let dir = tmp();
    let root = dir.path();
    write_file(root, "rt/p.md", b"policy");
    fs::create_dir(root.join("rules")).expect("mkdir rules");
    symlink("../rt", root.join("rules/link")).expect("dir symlink");
    force_dir_open_not_found("rules/link");
    let inventory = run(root).expect("reopened symlink target continues traversal");
    assert!(
        dir_open_seam_fired(),
        "the directory-open race seam must have fired"
    );
    assert!(
        pairs(&inventory).contains(&("rules/link/p.md".to_owned(), "policy")),
        "valid in-root replacement target is traversed"
    );
}

#[cfg(unix)]
#[test]
fn recursive_symlink_open_failure_rechecks_broken_link() {
    let dir = tmp();
    let root = dir.path();
    fs::create_dir(root.join("rules")).expect("mkdir rules");
    symlink("missing-target", root.join("rules/link")).expect("broken symlink");
    // White-box: a still-present symlink whose capability-relative target
    // resolution returns NotFound is reclassified as broken_symlink.
    let cap =
        cap_std::fs::Dir::open_ambient_dir(root, cap_std::ambient_authority()).expect("root cap");
    let error = reopen_raced_directory(&cap, "rules/link").expect_err("broken target must fail");
    assert_eq!(error.kind(), EK::BrokenSymlink);
    assert_eq!(error.locator(), Some("rules/link"));
    // End-to-end: a broken recursive symlink fails typed as broken_symlink.
    let error = run(root).expect_err("broken recursive symlink must fail");
    assert_eq!(error.kind(), EK::BrokenSymlink);
    assert_eq!(error.locator(), Some("rules/link"));
}

#[cfg(unix)]
#[test]
fn recursive_directory_disappearance_remains_entry_raced() {
    // A candidate whose directory open races to NotFound but is not a symlink
    // (vanished / became a non-symlink) stays entry_raced.
    let dir = tmp();
    let root = dir.path();
    write_file(root, "rules/real/p.md", b"policy");
    force_dir_open_not_found("rules/real");
    let error = run(root).expect_err("raced non-symlink directory must fail entry_raced");
    assert!(
        dir_open_seam_fired(),
        "the directory-open race seam must have fired"
    );
    assert_eq!(error.kind(), EK::EntryRaced);
    assert_eq!(error.locator(), Some("rules/real"));
    // A truly vanished candidate (non-following metadata NotFound) is raced too.
    let cap =
        cap_std::fs::Dir::open_ambient_dir(root, cap_std::ambient_authority()).expect("root cap");
    let error = reopen_raced_directory(&cap, "rules/absent").expect_err("vanished candidate");
    assert_eq!(error.kind(), EK::EntryRaced);
}

// ---------------------------------------------------------------------------
// §5 — Sort before fallible candidate classification.
// ---------------------------------------------------------------------------

#[cfg(unix)]
#[test]
fn fallible_recursive_classification_is_order_independent() {
    // Two broken recursive symlinks: whichever the OS enumerates first, the
    // sorted order must report the same first typed error and safe locator.
    let dir = tmp();
    let root = dir.path();
    fs::create_dir(root.join("rules")).expect("mkdir rules");
    symlink("missing-a", root.join("rules/b-link")).expect("broken b");
    symlink("missing-b", root.join("rules/a-link")).expect("broken a");
    let normal = run(root).expect_err("multiple broken symlinks must fail");
    let reversed = reversed_listing(|| run(root)).expect_err("reversed enumeration must fail");
    assert_eq!(normal.kind(), EK::BrokenSymlink);
    assert_eq!(
        normal, reversed,
        "first typed error is enumeration-order independent"
    );
    assert_eq!(
        normal.locator(),
        Some("rules/a-link"),
        "the sorted-order first candidate is reported"
    );
}

#[test]
fn fallible_suffix_classification_is_order_independent() {
    // Two root suffix entries that are directories: the sorted order must
    // report the same first typed error regardless of enumeration order.
    let dir = tmp();
    let root = dir.path();
    fs::create_dir(root.join("b.csproj")).expect("mkdir b");
    fs::create_dir(root.join("a.csproj")).expect("mkdir a");
    let normal = run(root).expect_err("suffix directories must fail");
    let reversed = reversed_listing(|| run(root)).expect_err("reversed enumeration must fail");
    assert_eq!(normal.kind(), EK::NonRegularEntry);
    assert_eq!(
        normal, reversed,
        "first typed error is enumeration-order independent"
    );
    assert_eq!(
        normal.locator(),
        Some("a.csproj"),
        "the sorted-order first suffix entry is reported"
    );
}

// ---------------------------------------------------------------------------
// §6 — Exercise read failure through the bounded read path.
// ---------------------------------------------------------------------------

#[test]
fn injected_reader_failure_exercises_bounded_read_path() {
    // The seam fails during `read_to_end` of an already-opened selected
    // regular file; it does not depend on permissions, so it runs even as
    // root or another privileged user.
    let dir = tmp();
    let root = dir.path();
    write_file(root, "CLAUDE.md", b"instructions");
    INJECT_READ_FAILURE.set(true);
    let result = run(root);
    assert!(
        !INJECT_READ_FAILURE.get(),
        "the read-failure seam must fire during read_to_end"
    );
    let error = result.expect_err("injected reader failure must fail read_failed");
    assert_eq!(error.kind(), EK::ReadFailed);
    assert_eq!(error.locator(), Some("CLAUDE.md"));
    // Production bounded reads still succeed once the seam is disarmed.
    let inventory = run(root).expect("normal read succeeds");
    assert!(pairs(&inventory).contains(&("CLAUDE.md".to_owned(), "instructions")));
}
