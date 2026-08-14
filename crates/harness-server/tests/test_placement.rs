use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
};

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
