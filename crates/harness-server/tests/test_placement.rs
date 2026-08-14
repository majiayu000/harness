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
            if path.extension().and_then(|value| value.to_str()) == Some("rs") {
                collect_declared_test_modules(&mut entries, src, &path)?;
            }
            if name == "tests.rs" || name.ends_with("_tests.rs") {
                record(&mut entries, "file", src, &path)?;
            }
        }
    }
    Ok(entries)
}

fn collect_declared_test_modules(
    entries: &mut BTreeSet<String>,
    src: &Path,
    source: &Path,
) -> anyhow::Result<()> {
    let contents = fs::read_to_string(source)?;
    let mut cfg_test = false;
    let mut path_override = None;

    for raw_line in contents.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with("//") {
            continue;
        }
        if line.starts_with("#[") {
            cfg_test |= is_test_cfg(line);
            if let Some(path) = path_attribute(line) {
                path_override = Some(path.to_owned());
            }
            continue;
        }

        if cfg_test {
            if let Some(module) = module_declaration(line) {
                let target = match path_override.as_deref() {
                    Some(path) => source.parent().unwrap_or(src).join(path.replace('\\', "/")),
                    None => resolve_module_path(source, module),
                };
                if target.is_file() {
                    record(entries, "file", src, &target)?;
                } else if target.is_dir() {
                    record(entries, "dir", src, &target)?;
                    collect_rust_files(entries, src, &target)?;
                }
            }
        }

        cfg_test = false;
        path_override = None;
    }
    Ok(())
}

fn is_test_cfg(attribute: &str) -> bool {
    attribute
        .chars()
        .filter(|character| !character.is_whitespace())
        .eq("#[cfg(test)]".chars())
}

fn path_attribute(attribute: &str) -> Option<&str> {
    let compact = attribute.trim();
    if !compact.starts_with("#[path") {
        return None;
    }
    let start = compact.find('"')? + 1;
    let end = compact[start..].find('"')? + start;
    Some(&compact[start..end])
}

fn module_declaration(line: &str) -> Option<&str> {
    let item = line.strip_suffix(';')?.trim();
    let marker = item.rfind("mod ")?;
    let visibility = &item[..marker];
    if !(visibility.is_empty()
        || visibility == "pub "
        || visibility.starts_with("pub(") && visibility.ends_with(") "))
    {
        return None;
    }
    let module = &item[marker + "mod ".len()..];
    let module = module.trim();
    (!module.is_empty()
        && module
            .chars()
            .all(|character| character == '_' || character.is_ascii_alphanumeric()))
    .then_some(module)
}

fn resolve_module_path(source: &Path, module: &str) -> PathBuf {
    let parent = source.parent().unwrap_or_else(|| Path::new(""));
    let filename = source.file_name().and_then(|value| value.to_str());
    let module_root = match filename {
        Some("lib.rs" | "main.rs" | "mod.rs") => parent.to_path_buf(),
        _ => parent.join(
            source
                .file_stem()
                .and_then(|value| value.to_str())
                .unwrap_or_default(),
        ),
    };
    let file = module_root.join(format!("{module}.rs"));
    if file.is_file() {
        file
    } else {
        module_root.join(module)
    }
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
    let canonical_src = fs::canonicalize(src)?;
    let canonical_path = fs::canonicalize(path)?;
    let relative = canonical_path
        .strip_prefix(canonical_src)?
        .to_string_lossy()
        .replace('\\', "/");
    entries.insert(format!("{kind} src/{relative}"));
    Ok(())
}
