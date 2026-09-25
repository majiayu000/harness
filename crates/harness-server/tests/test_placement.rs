use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
};

const PUBLIC_SERVER_MODULES: [&str; 4] = [
    "project_registry",
    "reconciliation",
    "server",
    "thread_manager",
];

const LEGACY_PLACEMENT: &str = include_str!("fixtures/src_test_placement.txt");

#[test]
fn src_test_placement_can_only_shrink() -> anyhow::Result<()> {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let src = manifest_dir.join("src");
    let current = scan_legacy_homes(&src)?;
    let expected = LEGACY_PLACEMENT
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(ToOwned::to_owned)
        .collect::<BTreeSet<_>>();
    let additions = current.difference(&expected).cloned().collect::<Vec<_>>();
    let retired = expected.difference(&current).cloned().collect::<Vec<_>>();
    assert!(
        additions.is_empty() && retired.is_empty(),
        "harness-server src/ test homes must match the shrinking inventory. Put new unit tests in the same file as `#[cfg(test)] mod tests`, and new API/route-contract tests in crates/harness-server/tests/. Remove migrated paths from the inventory so they cannot be recreated.\nNew entries:\n{}\nRetired entries still in the inventory:\n{}",
        additions.join("\n"),
        retired.join("\n")
    );
    Ok(())
}

#[test]
fn top_level_public_modules_match_the_supported_server_api() -> anyhow::Result<()> {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let source = fs::read_to_string(manifest_dir.join("src/lib.rs"))?;
    let syntax = syn::parse_file(&source)?;
    let actual = syntax
        .items
        .iter()
        .filter_map(|item| match item {
            syn::Item::Mod(module) if matches!(module.vis, syn::Visibility::Public(_)) => {
                Some(module.ident.to_string())
            }
            _ => None,
        })
        .collect::<BTreeSet<_>>();
    let expected = PUBLIC_SERVER_MODULES
        .into_iter()
        .map(str::to_owned)
        .collect::<BTreeSet<_>>();

    assert_eq!(
        actual, expected,
        "harness-server must expose only the supported top-level modules; add new public API through an existing facade or justify updating this allowlist"
    );
    let public_reexports = syntax
        .items
        .iter()
        .filter(|item| {
            matches!(
                item,
                syn::Item::Use(item_use)
                    if matches!(item_use.vis, syn::Visibility::Public(_))
            ) || matches!(
                item,
                syn::Item::ExternCrate(item_extern_crate)
                    if matches!(item_extern_crate.vis, syn::Visibility::Public(_))
            )
        })
        .count();
    assert_eq!(
        public_reexports, 0,
        "harness-server must not expose root-level re-exports that bypass the four-module API contract"
    );
    Ok(())
}

fn scan_legacy_homes(src: &Path) -> anyhow::Result<BTreeSet<String>> {
    let mut entries = BTreeSet::new();
    let mut pending = vec![src.to_path_buf()];
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(&directory)? {
            let path = entry?.path();
            let name = path
                .file_name()
                .and_then(|value| value.to_str())
                .unwrap_or_default();
            if path.is_dir() {
                if name == "tests" || name.ends_with("_tests") {
                    record(&mut entries, "dir", src, &path)?;
                    collect_rust_files(&mut entries, src, &path)?;
                    continue;
                }
                pending.push(path);
                continue;
            }
            if name == "tests.rs" || name.ends_with("_tests.rs") {
                record(&mut entries, "file", src, &path)?;
            }
        }
    }
    Ok(entries)
}

fn collect_rust_files(
    entries: &mut BTreeSet<String>,
    src: &Path,
    directory: &Path,
) -> anyhow::Result<()> {
    let mut pending = vec![directory.to_path_buf()];
    while let Some(current) = pending.pop() {
        for entry in fs::read_dir(&current)? {
            let path = entry?.path();
            if path.is_dir() {
                let name = path
                    .file_name()
                    .and_then(|value| value.to_str())
                    .unwrap_or_default();
                if name == "tests" || name.ends_with("_tests") {
                    record(entries, "dir", src, &path)?;
                }
                pending.push(path);
            } else if path.extension().and_then(|value| value.to_str()) == Some("rs") {
                record(entries, "file", src, &path)?;
            }
        }
    }
    Ok(())
}

fn record(
    entries: &mut BTreeSet<String>,
    kind: &str,
    src: &Path,
    path: &Path,
) -> anyhow::Result<()> {
    let relative = path.strip_prefix(src)?.to_string_lossy().replace('\\', "/");
    entries.insert(format!("{kind} src/{relative}"));
    Ok(())
}
