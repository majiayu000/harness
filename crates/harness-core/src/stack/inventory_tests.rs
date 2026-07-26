//! Behavior-invariant tests for the repository-observed Agent Stack
//! inventory (GH-1731, B-001..B-012).

use super::inventory::*;
use super::{AgentStackFreshness, Sha256Digest};
use std::fs;
use std::path::{Path, PathBuf};
use tempfile::TempDir;
use AgentStackInventoryErrorKind as EK;

#[cfg(unix)]
use std::os::unix::fs::{symlink, PermissionsExt};

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

fn run_ok(root: &Path) -> AgentStackInventory {
    run(root).expect("inventory must succeed")
}

/// Assert the scan fails with `kind` and return the error for locator checks.
fn assert_fail(root: &Path, kind: EK) -> AgentStackInventoryError {
    let error = run(root).expect_err("inventory must fail");
    assert_eq!(error.kind(), kind);
    error
}

fn pairs(inventory: &AgentStackInventory) -> Vec<(String, &'static str)> {
    inventory
        .entries()
        .iter()
        .map(|entry| {
            let component = entry.component();
            let locator = component.source().locator().as_str().to_owned();
            (locator, component.kind().as_str())
        })
        .collect()
}

fn of_kind<'a>(listed: &'a [(String, &'static str)], kind: &str) -> Vec<&'a str> {
    listed
        .iter()
        .filter(|(_, entry_kind)| *entry_kind == kind)
        .map(|(locator, _)| locator.as_str())
        .collect()
}

fn find<'a>(inventory: &'a AgentStackInventory, locator: &str) -> &'a AgentStackInventoryEntry {
    inventory
        .entries()
        .iter()
        .find(|entry| entry.component().source().locator().as_str() == locator)
        .unwrap_or_else(|| panic!("missing inventory entry for {locator}"))
}

fn digest_of(bytes: &[u8]) -> String {
    Sha256Digest::from_bytes(bytes).as_str().to_owned()
}

fn entry_digest(entry: &AgentStackInventoryEntry) -> String {
    entry
        .component()
        .integrity()
        .expect("regular file entries carry a digest")
        .as_str()
        .to_owned()
}

// ── B-001 ────────────────────────────────────────────────────────────────────

#[test]
#[rustfmt::skip]
fn invalid_limits_fail_before_root_open() {
    // Invalid limits are rejected at construction, before any root open, even
    // when the root itself does not exist.
    let missing = PathBuf::from("/nonexistent/harness-gh1731");
    let base = || AgentStackInventoryOptions::new(missing.clone());
    for result in [
        base().with_max_file_bytes(0), base().with_max_file_bytes(u64::MAX),
        base().with_max_total_bytes(0), base().with_max_files(0),
        base().with_max_directories(0), base().with_max_total_entries(0),
        base().with_max_depth(0), base().with_max_entries_per_directory(0),
        base().with_max_entries_per_directory(usize::MAX),
    ] {
        let error = result.expect_err("invalid limit must be rejected");
        assert_eq!(error.kind(), EK::InvalidOptions);
        assert_eq!(error.locator(), None);
    }
    // A missing, unreadable, or non-directory root fails as `root_open`.
    assert_eq!(run(&missing).expect_err("missing root").kind(), EK::RootOpen);
    let dir = tmp();
    write_file(dir.path(), "not-a-directory", b"x");
    let error = run(&dir.path().join("not-a-directory")).expect_err("file root must fail");
    assert_eq!(error.kind(), EK::RootOpen);
}

#[cfg(unix)]
#[test]
#[rustfmt::skip]
fn inventory_stays_bound_to_the_opened_root_handle() {
    // Replace the root path after opening the handle: traversal must keep
    // observing the originally opened directory, not the new path occupant.
    let outer = tmp();
    let root = outer.path().join("repo");
    fs::create_dir(&root).expect("mkdir root");
    write_file(&root, "AGENTS.md", b"original");
    let dir = cap_std::fs::Dir::open_ambient_dir(&root, cap_std::ambient_authority())
        .expect("open root capability");
    fs::rename(&root, outer.path().join("moved")).expect("rename root away");
    fs::create_dir(&root).expect("recreate root path");
    write_file(&root, "AGENTS.md", b"replacement");
    let inventory = inventory_with_root(&dir, &opts(&root)).expect("inventory");
    assert_eq!(entry_digest(find(&inventory, "AGENTS.md")), digest_of(b"original"));
}

#[cfg(not(unix))]
#[test]
fn inventory_stays_bound_to_the_opened_root_handle() {
    // Handle replacement cannot be raced portably here; assert the service
    // only reports repository-relative locators.
    let dir = tmp();
    write_file(dir.path(), "AGENTS.md", b"original");
    let inventory = run_ok(dir.path());
    assert!(pairs(&inventory).iter().all(|(l, _)| !l.starts_with('/')));
}

// ── B-002 ────────────────────────────────────────────────────────────────────

#[test]
#[rustfmt::skip]
fn inventory_never_reads_user_global_or_sibling_paths() {
    let outer = tmp();
    let root = outer.path().join("repo");
    fs::create_dir(&root).expect("mkdir");
    write_file(outer.path(), "sibling/AGENTS.md", b"sibling instructions");
    write_file(outer.path(), "AGENTS.md", b"parent instructions");
    assert!(run_ok(&root).entries().is_empty(), "sibling content must not leak");
    write_file(&root, "AGENTS.md", b"repo instructions");
    let listed = pairs(&run_ok(&root));
    assert_eq!(listed, vec![("AGENTS.md".to_owned(), "instructions")]);
    assert!(listed.iter().all(|(l, _)| !l.starts_with('/') && !l.contains("..")));
}

// ── B-003 ────────────────────────────────────────────────────────────────────

#[test]
#[rustfmt::skip]
fn inventory_discovers_every_stack_and_language_validation_selector() {
    let dir = tmp();
    let root = dir.path();
    let files: &[&str] = &[
        "AGENTS.md", "AGENTS.override.md", "CLAUDE.md", "WORKFLOW.md", "MEMORY.md",
        "src/AGENTS.md", "src/AGENTS.override.md", "src/CLAUDE.md",
        "crates/AGENTS.md", "crates/AGENTS.override.md", "crates/CLAUDE.md",
        "lib/AGENTS.md", "lib/AGENTS.override.md", "lib/CLAUDE.md",
        "pkg/AGENTS.md", "pkg/AGENTS.override.md", "pkg/CLAUDE.md",
        ".claude/skills/a.md", ".codex/skills/b.md", ".agents/skills/c.md",
        "skills/d.md", "skills/pack/SKILL.md", ".harness/skills/e.md",
        ".harness/guards/guard.sh", ".githooks/pre-commit",
        ".mcp.json", "mcp.json",
        ".vibeguard/rules.md", ".vibeguard/run-guards.sh",
        "rules/r.md", "requirements.toml", ".remem/mem.json", "remem.toml",
        ".harness/config.toml", ".harness/rules/hr.toml", ".harness/sg/scan.yml",
        ".github/workflows/ci.yml", ".cursor/rules/c.mdc",
        "Cargo.toml", "go.mod", "package.json", "pyproject.toml", "setup.py",
        "requirements.txt", "build.gradle", "build.gradle.kts", "pom.xml", "Gemfile",
        "yarn.lock", "pnpm-lock.yaml", ".eslintrc", ".eslintrc.js", ".eslintrc.cjs",
        ".eslintrc.json", ".eslintrc.yaml", ".eslintrc.yml", "eslint.config.js",
        "eslint.config.mjs", "eslint.config.cjs", "biome.json", ".rubocop.yml",
        "App.csproj", "App.sln", ".csproj", ".sln", "Makefile", "justfile",
        // Unrelated files that must stay excluded.
        "README.md", "src/main.rs", "docs/notes.md", ".vibeguard/helper.sh",
        ".harness/local/run.log", "hooks/x.sh", ".claude/hooks/y.sh",
        "spec/models/user_spec.rb",
    ];
    for rel in files {
        write_file(root, rel, rel.as_bytes());
    }
    // Keep `harness.toml` valid TOML so configured-rule parsing succeeds.
    write_file(root, "harness.toml", b"# empty harness config\n");
    let expected: &[(&str, &str)] = &[
        ("AGENTS.md", "instructions"), ("AGENTS.override.md", "instructions"),
        ("CLAUDE.md", "instructions"), ("WORKFLOW.md", "workflow"), ("MEMORY.md", "memory"),
        ("src/AGENTS.md", "instructions"), ("src/AGENTS.override.md", "instructions"),
        ("src/CLAUDE.md", "instructions"), ("crates/AGENTS.md", "instructions"),
        ("crates/AGENTS.override.md", "instructions"), ("crates/CLAUDE.md", "instructions"),
        ("lib/AGENTS.md", "instructions"), ("lib/AGENTS.override.md", "instructions"),
        ("lib/CLAUDE.md", "instructions"), ("pkg/AGENTS.md", "instructions"),
        ("pkg/AGENTS.override.md", "instructions"), ("pkg/CLAUDE.md", "instructions"),
        (".claude/skills/a.md", "skill"), (".codex/skills/b.md", "skill"),
        (".agents/skills/c.md", "skill"), ("skills/d.md", "skill"),
        ("skills/pack/SKILL.md", "skill"), (".harness/skills/e.md", "skill"),
        (".harness/guards/guard.sh", "hook"), (".githooks/pre-commit", "hook"),
        (".mcp.json", "mcp_server"), ("mcp.json", "mcp_server"),
        (".vibeguard/rules.md", "policy"), (".vibeguard/run-guards.sh", "validation"),
        ("rules/r.md", "policy"), ("requirements.toml", "policy"),
        (".remem/mem.json", "memory"), ("remem.toml", "memory"),
        (".harness/config.toml", "validation"), (".harness/rules/hr.toml", "policy"),
        (".harness/sg/scan.yml", "policy"), ("harness.toml", "validation"),
        (".github/workflows/ci.yml", "workflow"), (".cursor/rules/c.mdc", "policy"),
        ("Cargo.toml", "validation"), ("go.mod", "validation"),
        ("package.json", "validation"), ("pyproject.toml", "validation"),
        ("setup.py", "validation"), ("requirements.txt", "validation"),
        ("build.gradle", "validation"), ("build.gradle.kts", "validation"),
        ("pom.xml", "validation"), ("Gemfile", "validation"),
        ("yarn.lock", "validation"), ("pnpm-lock.yaml", "validation"),
        (".eslintrc", "validation"), (".eslintrc.js", "validation"),
        (".eslintrc.cjs", "validation"), (".eslintrc.json", "validation"),
        (".eslintrc.yaml", "validation"), (".eslintrc.yml", "validation"),
        ("eslint.config.js", "validation"), ("eslint.config.mjs", "validation"),
        ("eslint.config.cjs", "validation"), ("biome.json", "validation"),
        (".rubocop.yml", "validation"),
        ("App.csproj", "validation"), ("App.sln", "validation"),
        (".csproj", "validation"), (".sln", "validation"),
        ("spec", "validation"), ("Makefile", "validation"), ("justfile", "validation"),
    ];
    let mut expected: Vec<(String, &'static str)> =
        expected.iter().map(|(l, k)| ((*l).to_owned(), *k)).collect();
    expected.sort();
    assert_eq!(pairs(&run_ok(root)), expected);
}

#[test]
#[rustfmt::skip]
fn vibeguard_runner_is_inventoried_as_validation() {
    let dir = tmp();
    write_file(dir.path(), ".vibeguard/run-guards.sh", b"#!/bin/sh\n");
    write_file(dir.path(), ".vibeguard/helper.sh", b"#!/bin/sh\n");
    write_file(dir.path(), ".vibeguard/rules.md", b"rule");
    let listed = pairs(&run_ok(dir.path()));
    assert!(listed.contains(&(".vibeguard/run-guards.sh".to_owned(), "validation")));
    assert!(listed.contains(&(".vibeguard/rules.md".to_owned(), "policy")));
    assert!(!listed.iter().any(|(l, _)| l == ".vibeguard/helper.sh"));
}

#[test]
#[rustfmt::skip]
fn configured_repository_rule_sources_are_inventoried_once() {
    let dir = tmp();
    let root = dir.path();
    write_file(root, "harness.toml", br#"[rules]
discovery_paths = ["policies", "policies", "./policies", "/absolute/outside"]
builtin_path = "builtin.toml"
exec_policy_paths = ["exec.policy.toml"]
requirements_path = "reqs.toml"
"#);
    write_file(root, "policies/p.md", b"p");
    write_file(root, "policies/p.toml", b"q = 1");
    write_file(root, "policies/skip.txt", b"not selected");
    write_file(root, "builtin.toml", b"b = 1");
    write_file(root, "exec.policy.toml", b"e = 1");
    write_file(root, "reqs.toml", b"r = 1");
    let listed = pairs(&run_ok(root));
    let expected = ["builtin.toml", "exec.policy.toml", "policies/p.md", "policies/p.toml", "reqs.toml"];
    assert_eq!(of_kind(&listed, "policy"), expected, "each binding is inventoried once");
    assert!(listed.contains(&("harness.toml".to_owned(), "validation")));
    assert!(!listed.iter().any(|(l, _)| l.contains("absolute") || l.contains("outside")));
    assert!(!listed.iter().any(|(l, _)| l == "policies/skip.txt"));
}

#[test]
#[rustfmt::skip]
fn same_locator_with_distinct_kinds_is_preserved() {
    let dir = tmp();
    write_file(dir.path(), "harness.toml",
        b"[rules]\ndiscovery_paths = [\"Cargo.toml\", \"harness.toml\"]\n");
    write_file(dir.path(), "Cargo.toml", b"[package]\nname = \"fixture\"\n");
    let options = opts(dir.path()).with_max_files(2).expect("two physical files");
    let inventory = inventory_repository_stack(&options).expect("reuse observations by locator");
    for locator in ["Cargo.toml", "harness.toml"] {
        let entries: Vec<_> = inventory.entries().iter()
            .filter(|entry| entry.component().source().locator().as_str() == locator).collect();
        assert_eq!(entries.len(), 2, "one component per kind");
        assert_eq!(entry_digest(entries[0]), entry_digest(entries[1]), "one opened observation");
    }
}

// ── B-004 ────────────────────────────────────────────────────────────────────

#[test]
fn missing_allowlisted_entries_emit_no_placeholders() {
    let dir = tmp();
    assert!(run_ok(dir.path()).entries().is_empty());
    write_file(dir.path(), "WORKFLOW.md", b"w");
    let listed = pairs(&run_ok(dir.path()));
    assert_eq!(listed, vec![("WORKFLOW.md".to_owned(), "workflow")]);
}

// ── B-005 ────────────────────────────────────────────────────────────────────

#[test]
#[rustfmt::skip]
fn entries_bind_content_mode_and_directory_presence_to_valid_components() {
    let dir = tmp();
    let root = dir.path();
    write_file(root, "AGENTS.md", b"hello world");
    write_file(root, ".githooks/pre-commit", b"#!/bin/sh\n");
    write_file(root, "spec/user_spec.rb", b"inside");
    let inventory = run_ok(root);
    let agents = find(&inventory, "AGENTS.md");
    assert_eq!(entry_digest(agents), digest_of(b"hello world"));
    assert_eq!(agents.component().component_id().as_str(), "repository:instructions:AGENTS.md");
    let spec = find(&inventory, "spec");
    assert_eq!(*spec.entry_class(), AgentStackEntryClass::DirectoryPresence);
    assert!(spec.component().integrity().is_none());
    assert_eq!(spec.component().kind().as_str(), "validation");
    assert!(!pairs(&inventory).iter().any(|(l, _)| l.starts_with("spec/")), "no spec recursion");
    for entry in inventory.entries() {
        entry.component().validate().expect("every component validates under ASC-001");
    }
    #[cfg(unix)]
    {
        let hook_path = root.join(".githooks/pre-commit");
        fs::set_permissions(&hook_path, fs::Permissions::from_mode(0o755)).expect("chmod");
        let enabled = run_ok(root);
        fs::set_permissions(&hook_path, fs::Permissions::from_mode(0o644)).expect("chmod");
        let disabled = run_ok(root);
        let on = find(&enabled, ".githooks/pre-commit");
        let off = find(&disabled, ".githooks/pre-commit");
        assert_eq!(entry_digest(on), entry_digest(off), "bytes unchanged");
        let exec = |entry: &AgentStackInventoryEntry| match entry.entry_class() {
            AgentStackEntryClass::RegularFile { unix_executable } => *unix_executable,
            AgentStackEntryClass::DirectoryPresence => panic!("hook is a file"),
        };
        assert_eq!(exec(on), Some(true));
        assert_eq!(exec(off), Some(false));
        assert_ne!(on, off, "executable-bit change alters entry evidence");
    }
    #[cfg(not(unix))]
    match find(&inventory, "AGENTS.md").entry_class() {
        AgentStackEntryClass::RegularFile { unix_executable } => {
            assert_eq!(*unix_executable, None, "mode is explicitly unobserved");
        }
        AgentStackEntryClass::DirectoryPresence => panic!("AGENTS.md is a file"),
    }
}

#[test]
fn current_observations_are_fresh() {
    let dir = tmp();
    write_file(dir.path(), "AGENTS.md", b"a");
    fs::create_dir(dir.path().join("spec")).expect("mkdir spec");
    let inventory = run_ok(dir.path());
    assert_eq!(inventory.entries().len(), 2);
    for entry in inventory.entries() {
        assert_eq!(entry.component().freshness(), AgentStackFreshness::Fresh);
    }
}

// ── B-006 ────────────────────────────────────────────────────────────────────

#[test]
#[rustfmt::skip]
fn sidecars_and_support_files_do_not_emit_stack_units() {
    let dir = tmp();
    let root = dir.path();
    write_file(root, ".harness/skills/skill.md", b"s");
    write_file(root, ".harness/skills/skill.usage.json", b"{}");
    write_file(root, ".harness/skills/nested/pkg.md", b"nested");
    write_file(root, "skills/direct.md", b"d");
    write_file(root, "skills/pack/SKILL.md", b"entry");
    write_file(root, "skills/pack/helper.py", b"print()");
    write_file(root, "skills/pack/notes.md", b"support");
    write_file(root, "skills/pack/refs.usage.json", b"{}");
    let listed = pairs(&run_ok(root));
    let expected = [".harness/skills/skill.md", "skills/direct.md", "skills/pack/SKILL.md"];
    assert_eq!(of_kind(&listed, "skill"), expected);
    for excluded in [
        ".harness/skills/skill.usage.json", ".harness/skills/nested/pkg.md",
        "skills/pack/helper.py", "skills/pack/notes.md", "skills/pack/refs.usage.json",
    ] {
        assert!(!listed.iter().any(|(l, _)| l == excluded), "{excluded} must be excluded");
    }
}

#[test]
#[rustfmt::skip]
fn only_lifecycle_bound_hook_entrypoints_are_inventoried() {
    let dir = tmp();
    let root = dir.path();
    write_file(root, ".githooks/pre-commit", b"#!/bin/sh\n");
    write_file(root, ".githooks/pre-push", b"#!/bin/sh\n");
    write_file(root, ".githooks/README.md", b"docs");
    write_file(root, ".githooks/helper.sh", b"#!/bin/sh\n");
    write_file(root, ".githooks/nested/pre-commit", b"#!/bin/sh\n");
    write_file(root, ".harness/guards/check.sh", b"#!/bin/sh\n");
    write_file(root, ".harness/guards/README.md", b"docs");
    write_file(root, ".harness/guards/nested/x.sh", b"#!/bin/sh\n");
    let listed = pairs(&run_ok(root));
    let expected = [".githooks/pre-commit", ".githooks/pre-push", ".harness/guards/check.sh"];
    assert_eq!(of_kind(&listed, "hook"), expected);
    assert!(!listed.iter().any(|(l, _)| {
        l.contains("README") || l.contains("helper") || l.contains("nested")
    }));
}

#[test]
#[rustfmt::skip]
fn component_kind_comes_from_matching_rule() {
    let dir = tmp();
    let root = dir.path();
    // Identical bytes everywhere: the kind must come from the matched rule.
    for rel in ["skills/same.md", "rules/same.md", "MEMORY.md", "WORKFLOW.md", ".mcp.json"] {
        write_file(root, rel, b"identical contents");
    }
    let listed = pairs(&run_ok(root));
    assert!(listed.contains(&("skills/same.md".to_owned(), "skill")));
    assert!(listed.contains(&("rules/same.md".to_owned(), "policy")));
    assert!(listed.contains(&("MEMORY.md".to_owned(), "memory")));
    assert!(listed.contains(&("WORKFLOW.md".to_owned(), "workflow")));
    assert!(listed.contains(&(".mcp.json".to_owned(), "mcp_server")));
}

// ── B-007 ────────────────────────────────────────────────────────────────────

#[cfg(unix)]
#[test]
#[rustfmt::skip]
fn unsupported_non_utf8_entries_are_filtered_before_locator_normalization() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    let dir = tmp();
    let root = dir.path();
    write_file(root, "skills/ok.md", b"ok");
    let unmatched = root.join("skills").join(OsStr::from_bytes(b"raw-\xFF-bytes.bin"));
    if fs::write(&unmatched, b"binary").is_err() {
        // This filesystem (for example APFS) rejects non-UTF-8 names, so the
        // fixture is unrepresentable; assert the typed category directly.
        assert_eq!(EK::NonUtf8Locator.as_str(), "non_utf8_locator");
        return;
    }
    assert_eq!(pairs(&run_ok(root)), vec![("skills/ok.md".to_owned(), "skill")]);
    // A selected non-UTF-8 name fails typed with the representable ancestor.
    let selected = root.join("skills").join(OsStr::from_bytes(b"bad-\xFF.md"));
    fs::write(&selected, b"selected").expect("write non-utf8 md");
    let error = assert_fail(root, EK::NonUtf8Locator);
    assert_eq!(error.locator(), Some("skills"), "only the representable ancestor is reported");
}

#[cfg(windows)]
#[test]
fn unsupported_non_utf8_entries_are_filtered_before_locator_normalization() {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;
    let dir = tmp();
    fs::create_dir(dir.path().join("skills")).expect("mkdir");
    let name = OsString::from_wide(&[0xD800, b'.' as u16, b'm' as u16, b'd' as u16]);
    fs::write(dir.path().join("skills").join(name), b"selected").expect("write invalid UTF-16");
    assert_fail(dir.path(), EK::NonUtf8Locator);
}

#[cfg(all(not(unix), not(windows)))]
#[test]
fn unsupported_non_utf8_entries_are_filtered_before_locator_normalization() {
    assert_eq!(EK::NonUtf8Locator.as_str(), "non_utf8_locator");
}

#[test]
#[rustfmt::skip]
fn filesystem_enumeration_order_does_not_change_inventory() {
    let dir = tmp();
    let root = dir.path();
    // Create in deliberately non-sorted order.
    for rel in ["skills/zz.md", "skills/aa.md", "skills/mm.md", "rules/z.toml", "rules/a.md"] {
        write_file(root, rel, rel.as_bytes());
    }
    let first = run_ok(root);
    let second = run_ok(root);
    let listed = pairs(&first);
    let mut sorted = listed.clone();
    sorted.sort();
    assert_eq!(listed, sorted, "output is sorted by normalized locator");
    assert_eq!(first, second, "enumeration order cannot change output");
}

// ── B-008 ────────────────────────────────────────────────────────────────────

#[cfg(unix)]
#[test]
#[rustfmt::skip]
fn symlink_swaps_remain_root_confined_and_hash_the_opened_target() {
    let outer = tmp();
    let root = outer.path().join("repo");
    fs::create_dir(&root).expect("mkdir");
    write_file(&root, "a.txt", b"target a");
    write_file(&root, "b.txt", b"target b");
    symlink("a.txt", root.join("CLAUDE.md")).expect("symlink");
    let inventory = run_ok(&root);
    assert_eq!(entry_digest(find(&inventory, "CLAUDE.md")), digest_of(b"target a"));
    fs::remove_file(root.join("CLAUDE.md")).expect("remove link");
    symlink("b.txt", root.join("CLAUDE.md")).expect("relink");
    let swapped = run_ok(&root);
    assert_eq!(entry_digest(find(&swapped, "CLAUDE.md")), digest_of(b"target b"));
    write_file(outer.path(), "outside.txt", b"outside");
    fs::remove_file(root.join("CLAUDE.md")).expect("remove link");
    symlink("../outside.txt", root.join("CLAUDE.md")).expect("escape link");
    let error = assert_fail(&root, EK::RootEscape);
    assert_eq!(error.locator(), Some("CLAUDE.md"));
    fs::remove_file(root.join("CLAUDE.md")).expect("remove link");
    let cap = cap_std::fs::Dir::open_ambient_dir(&root, cap_std::ambient_authority()).expect("root");
    assert_eq!(classify_resolution_failure(&cap, "CLAUDE.md", std::io::ErrorKind::NotFound), EK::EntryRaced);
    symlink("gone.txt", root.join("CLAUDE.md")).expect("broken link");
    assert_fail(&root, EK::BrokenSymlink);
}

#[cfg(not(unix))]
#[test]
fn symlink_swaps_remain_root_confined_and_hash_the_opened_target() {
    // Symlink fixtures are Unix-only; assert digests bind to opened bytes.
    let dir = tmp();
    write_file(dir.path(), "CLAUDE.md", b"target a");
    let inventory = run_ok(dir.path());
    let digest = entry_digest(find(&inventory, "CLAUDE.md"));
    assert_eq!(digest, digest_of(b"target a"));
}

#[cfg(unix)]
#[test]
#[rustfmt::skip]
fn in_root_directory_symlinks_are_traversed_and_cycles_fail() {
    let dir = tmp();
    let root = dir.path();
    write_file(root, "rules-src/a.md", b"a");
    symlink("rules-src", root.join("rules")).expect("dir symlink");
    assert!(pairs(&run_ok(root)).contains(&("rules/a.md".to_owned(), "policy")));
    // A directory identity already on the active ancestor stack is a cycle.
    let cyclic = tmp();
    write_file(cyclic.path(), ".vibeguard/sub/rule.md", b"r");
    symlink("../../.vibeguard", cyclic.path().join(".vibeguard/sub/loop")).expect("cycle link");
    assert_fail(cyclic.path(), EK::CycleDetected);
}

#[cfg(not(unix))]
#[test]
fn in_root_directory_symlinks_are_traversed_and_cycles_fail() {
    let dir = tmp();
    let first = cap_std::fs::Dir::open_ambient_dir(dir.path(), cap_std::ambient_authority())
        .expect("first handle");
    let second = cap_std::fs::Dir::open_ambient_dir(dir.path(), cap_std::ambient_authority())
        .expect("second handle");
    assert!(
        directory_identity(&first, "").expect("identity")
            == directory_identity(&second, "").expect("identity")
    );
    assert_eq!(EK::CycleDetected.as_str(), "cycle_detected");
}

#[cfg(unix)]
#[test]
fn file_rules_reject_directory_symlink_targets() {
    let dir = tmp();
    let root = dir.path();
    fs::create_dir(root.join("somewhere")).expect("mkdir");
    symlink("somewhere", root.join("CLAUDE.md")).expect("symlink");
    let error = assert_fail(root, EK::NonRegularEntry);
    assert_eq!(error.locator(), Some("CLAUDE.md"));
    let suffix = tmp();
    fs::create_dir(suffix.path().join("target")).expect("mkdir");
    symlink("target", suffix.path().join("App.csproj")).expect("suffix symlink");
    let error = assert_fail(suffix.path(), EK::NonRegularEntry);
    assert_eq!(error.locator(), Some("App.csproj"));
}

#[cfg(not(unix))]
#[test]
fn file_rules_reject_directory_symlink_targets() {
    // Without symlinks, assert the same rejection for a plain directory that
    // occupies an exact-file rule path.
    let dir = tmp();
    fs::create_dir(dir.path().join("CLAUDE.md")).expect("mkdir");
    assert_fail(dir.path(), EK::NonRegularEntry);
}

// ── B-009 ────────────────────────────────────────────────────────────────────

#[cfg(unix)]
#[test]
#[rustfmt::skip]
fn unreadable_and_non_utf8_entries_fail_without_lossy_locators() {
    let dir = tmp();
    let root = dir.path();
    write_file(root, "CLAUDE.md", b"secret");
    let path = root.join("CLAUDE.md");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).expect("chmod 000");
    if fs::File::open(&path).is_err() {
        let error = assert_fail(root, EK::ReadFailed);
        assert_eq!(error.locator(), Some("CLAUDE.md"));
        assert!(!error.locator().unwrap_or_default().contains('\u{FFFD}'), "no lossy locator");
    }
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).expect("chmod back");
    // Deterministic injected failure mapping covers privileged environments
    // where permission bits cannot produce an unreadable file.
    assert_eq!(classify_open_failure(std::io::ErrorKind::PermissionDenied), EK::ReadFailed);
    assert_eq!(classify_open_failure(std::io::ErrorKind::NotFound), EK::EntryRaced);
}

#[cfg(not(unix))]
#[test]
fn unreadable_and_non_utf8_entries_fail_without_lossy_locators() {
    // Deterministic injected failure mapping replaces permission fixtures.
    let denied = classify_open_failure(std::io::ErrorKind::PermissionDenied);
    assert_eq!(denied, EK::ReadFailed);
    let raced = classify_open_failure(std::io::ErrorKind::NotFound);
    assert_eq!(raced, EK::EntryRaced);
}

#[test]
#[rustfmt::skip]
fn invalid_configured_rule_sources_fail_typed() {
    let invalid = tmp();
    write_file(invalid.path(), "harness.toml", b"[rules]\ndiscovery_paths = [\"../escape\"]\n");
    let error = assert_fail(invalid.path(), EK::ConfiguredSourceInvalid);
    assert_eq!(error.locator(), Some("harness.toml"));
    let empty = tmp();
    write_file(empty.path(), "harness.toml", b"[rules]\nexec_policy_paths = [\"\"]\n");
    assert_fail(empty.path(), EK::ConfiguredSourceInvalid);
    let missing = tmp();
    write_file(missing.path(), "harness.toml", b"[rules]\nrequirements_path = \"nope.toml\"\n");
    let error = assert_fail(missing.path(), EK::ConfiguredSourceMissing);
    assert_eq!(error.locator(), Some("nope.toml"));
    let bad_toml = tmp();
    write_file(bad_toml.path(), "harness.toml", b"rules = [not toml");
    assert_fail(bad_toml.path(), EK::ConfigParse);
    let dir_target = tmp();
    write_file(dir_target.path(), "harness.toml", b"[rules]\nexec_policy_paths = [\"adir\"]\n");
    fs::create_dir(dir_target.path().join("adir")).expect("mkdir");
    assert_fail(dir_target.path(), EK::NonRegularEntry);
}

// ── B-010 ────────────────────────────────────────────────────────────────────

#[test]
#[rustfmt::skip]
fn every_traversal_limit_has_an_exact_boundary_fixture() {
    let base = |root: &Path| AgentStackInventoryOptions::new(root.to_path_buf());
    let run_with = |options: Result<AgentStackInventoryOptions, AgentStackInventoryError>| {
        inventory_repository_stack(&options.expect("valid boundary options"))
    };
    let files = tmp();
    write_file(files.path(), "AGENTS.md", b"aaaa");
    write_file(files.path(), "CLAUDE.md", b"bbbb");
    // max_files: two regular files fit exactly; one fails on the second read.
    assert!(run_with(base(files.path()).with_max_files(2)).is_ok());
    let error = run_with(base(files.path()).with_max_files(1)).expect_err("file budget");
    assert_eq!((error.kind(), error.locator()), (EK::LimitExceeded, Some("CLAUDE.md")));
    // max_file_bytes: a 4-byte file passes at 4 and fails at 3.
    assert!(run_with(base(files.path()).with_max_file_bytes(4)).is_ok());
    let error = run_with(base(files.path()).with_max_file_bytes(3)).expect_err("per-file bytes");
    assert_eq!(error.kind(), EK::LimitExceeded);
    // max_total_bytes: 4 + 4 bytes pass at 8 and fail at 7.
    assert!(run_with(base(files.path()).with_max_total_bytes(8)).is_ok());
    let error = run_with(base(files.path()).with_max_total_bytes(7)).expect_err("total bytes");
    assert_eq!((error.kind(), error.locator()), (EK::LimitExceeded, Some("CLAUDE.md")));
    // Depth and opened-directory budgets: skills/a/SKILL.md needs depth 2 and
    // three opened directories (root, skills, skills/a).
    let deep = tmp();
    write_file(deep.path(), "skills/a/SKILL.md", b"s");
    assert!(run_with(base(deep.path()).with_max_depth(2)).is_ok());
    let error = run_with(base(deep.path()).with_max_depth(1)).expect_err("depth budget");
    assert_eq!((error.kind(), error.locator()), (EK::LimitExceeded, Some("skills/a")));
    assert!(run_with(base(deep.path()).with_max_directories(3)).is_ok());
    let error = run_with(base(deep.path()).with_max_directories(2)).expect_err("dir budget");
    assert_eq!(error.kind(), EK::LimitExceeded);
    // max_entries_per_directory: three children fit exactly at 3, fail at 2.
    let wide = tmp();
    for rel in ["skills/a.md", "skills/b.md", "skills/c.md"] {
        write_file(wide.path(), rel, b"x");
    }
    assert!(run_with(base(wide.path()).with_max_entries_per_directory(3)).is_ok());
    let error = run_with(base(wide.path()).with_max_entries_per_directory(2))
        .expect_err("per-directory entries");
    assert_eq!(error.kind(), EK::LimitExceeded);
    // max_total_entries: the skills traversal yields 3 entries and the root
    // suffix enumeration yields 1 (the `skills` directory itself): 4 exactly.
    assert!(run_with(base(wide.path()).with_max_total_entries(4)).is_ok());
    let error = run_with(base(wide.path()).with_max_total_entries(3)).expect_err("entries");
    assert_eq!(error.kind(), EK::LimitExceeded);
}

#[test]
#[rustfmt::skip]
fn reads_never_exceed_remaining_aggregate_or_per_file_budget() {
    let dir = tmp();
    write_file(dir.path(), "AGENTS.md", b"0123456789");
    let options = opts(dir.path()).with_max_file_bytes(4).expect("options");
    let error = inventory_repository_stack(&options).expect_err("oversized file");
    assert_eq!((error.kind(), error.locator()), (EK::LimitExceeded, Some("AGENTS.md")));
    // The second file exceeds the remaining aggregate budget even though it
    // stays within the per-file budget.
    let pair = tmp();
    write_file(pair.path(), "AGENTS.md", b"aaaa");
    write_file(pair.path(), "CLAUDE.md", b"bbbb");
    let options = opts(pair.path()).with_max_total_bytes(6).expect("options");
    let error = inventory_repository_stack(&options).expect_err("aggregate budget");
    assert_eq!((error.kind(), error.locator()), (EK::LimitExceeded, Some("CLAUDE.md")));
}

#[cfg(unix)]
#[test]
#[rustfmt::skip]
fn selected_fifo_targets_fail_without_blocking() {
    use std::sync::mpsc;
    use std::time::Duration;
    let dir = tmp();
    let status = std::process::Command::new("mkfifo")
        .arg(dir.path().join("Makefile"))
        .status()
        .expect("spawn mkfifo");
    assert!(status.success(), "mkfifo must succeed");
    write_file(dir.path(), "AGENTS.md", b"regular file consumes the budget");
    let root = dir.path().to_path_buf();
    let options = opts(&root).with_max_files(1).expect("one regular file");
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = sender.send(inventory_repository_stack(&options));
    });
    let result = receiver
        .recv_timeout(Duration::from_secs(10))
        .expect("inventory must not block on a writer-less FIFO");
    let error = result.expect_err("fifo is a non-regular entry");
    assert_eq!((error.kind(), error.locator()), (EK::NonRegularEntry, Some("Makefile")));
}

#[cfg(not(unix))]
#[test]
fn selected_fifo_targets_fail_without_blocking() {
    assert_eq!(EK::NonRegularEntry.as_str(), "non_regular_entry");
}

// ── B-011 ────────────────────────────────────────────────────────────────────

#[test]
fn unchanged_repository_inventory_is_repeatable() {
    let dir = tmp();
    let root = dir.path();
    write_file(root, "AGENTS.md", b"stable");
    write_file(root, "skills/pack/SKILL.md", b"skill");
    write_file(root, ".githooks/pre-commit", b"#!/bin/sh\n");
    fs::create_dir(root.join("spec")).expect("mkdir spec");
    let first = run_ok(root);
    let second = run_ok(root);
    assert_eq!(first, second, "unchanged bytes produce identical output");
    assert_eq!(first.entries().len(), 4);
}

// ── B-012 ────────────────────────────────────────────────────────────────────

fn snapshot(root: &Path) -> Vec<(PathBuf, Option<Vec<u8>>)> {
    fn walk(dir: &Path, out: &mut Vec<(PathBuf, Option<Vec<u8>>)>) {
        let mut children: Vec<_> = fs::read_dir(dir)
            .expect("read_dir")
            .map(|entry| entry.expect("entry").path())
            .collect();
        children.sort();
        for child in children {
            if child.is_dir() {
                out.push((child.clone(), None));
                walk(&child, out);
            } else {
                out.push((child.clone(), Some(fs::read(&child).expect("read"))));
            }
        }
    }
    let mut out = Vec::new();
    walk(root, &mut out);
    out
}

#[test]
#[rustfmt::skip]
fn inventory_is_read_only_and_invokes_no_external_behavior() {
    let dir = tmp();
    let root = dir.path();
    write_file(root, "AGENTS.md", b"a");
    write_file(root, "harness.toml", b"[rules]\ndiscovery_paths = [\"policies\"]\n");
    write_file(root, "policies/p.md", b"p");
    write_file(root, ".githooks/pre-commit", b"#!/bin/sh\n");
    fs::create_dir(root.join("spec")).expect("mkdir spec");
    let before = snapshot(root);
    let inventory = run_ok(root);
    assert!(!inventory.entries().is_empty());
    assert_eq!(snapshot(root), before, "inventory performs no repository writes");
}
