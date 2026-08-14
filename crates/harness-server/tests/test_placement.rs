use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
};
use syn::{punctuated::Punctuated, Attribute, Expr, Item, Lit, Meta, Token};

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
    let syntax = syn::parse_file(&contents)?;
    let source_dir = source.parent().unwrap_or(src);
    collect_test_module_items(
        entries,
        src,
        source_dir,
        &module_root(source),
        &syntax.items,
        false,
    )
}

fn collect_test_module_items(
    entries: &mut BTreeSet<String>,
    src: &Path,
    path_dir: &Path,
    module_dir: &Path,
    items: &[Item],
    inherited_test: bool,
) -> anyhow::Result<()> {
    for item in items {
        let Item::Mod(module) = item else {
            continue;
        };
        let test_only = inherited_test || module.attrs.iter().any(is_test_cfg);
        if let Some((_, nested_items)) = &module.content {
            let nested_module_dir = module_dir.join(module.ident.to_string());
            collect_test_module_items(
                entries,
                src,
                &nested_module_dir,
                &nested_module_dir,
                nested_items,
                test_only,
            )?;
            continue;
        }

        let test_path = cfg_attr_test_path(&module.attrs).map(|path| path_dir.join(path));
        let target = test_path.or_else(|| {
            test_only.then(|| {
                path_attribute(&module.attrs).map_or_else(
                    || resolve_module_path(module_dir, &module.ident.to_string()),
                    |path| path_dir.join(path),
                )
            })
        });
        if let Some(target) = target {
            record_module_target(entries, src, &target)?;
        }
    }
    Ok(())
}

fn is_test_cfg(attribute: &Attribute) -> bool {
    attribute.path().is_ident("cfg")
        && matches!(attribute.parse_args::<Meta>(), Ok(Meta::Path(path)) if path.is_ident("test"))
}

fn path_attribute(attributes: &[Attribute]) -> Option<PathBuf> {
    attributes
        .iter()
        .find(|attribute| attribute.path().is_ident("path"))
        .and_then(attribute_path_value)
}

fn cfg_attr_test_path(attributes: &[Attribute]) -> Option<PathBuf> {
    attributes
        .iter()
        .filter(|attribute| attribute.path().is_ident("cfg_attr"))
        .find_map(|attribute| {
            let arguments = attribute
                .parse_args_with(Punctuated::<Meta, Token![,]>::parse_terminated)
                .ok()?;
            let mut arguments = arguments.iter();
            let condition = arguments.next()?;
            if !matches!(condition, Meta::Path(path) if path.is_ident("test")) {
                return None;
            }
            arguments.find_map(meta_path_value)
        })
}

fn attribute_path_value(attribute: &Attribute) -> Option<PathBuf> {
    meta_path_value(&attribute.meta)
}

fn meta_path_value(meta: &Meta) -> Option<PathBuf> {
    let Meta::NameValue(value) = meta else {
        return None;
    };
    if !value.path.is_ident("path") {
        return None;
    }
    let Expr::Lit(value) = &value.value else {
        return None;
    };
    let Lit::Str(value) = &value.lit else {
        return None;
    };
    Some(PathBuf::from(value.value()))
}

fn module_root(source: &Path) -> PathBuf {
    let parent = source.parent().unwrap_or_else(|| Path::new(""));
    let filename = source.file_name().and_then(|value| value.to_str());
    match filename {
        Some("lib.rs" | "main.rs" | "mod.rs") => parent.to_path_buf(),
        _ => parent.join(
            source
                .file_stem()
                .and_then(|value| value.to_str())
                .unwrap_or_default(),
        ),
    }
}

fn resolve_module_path(module_root: &Path, module: &str) -> PathBuf {
    let file = module_root.join(format!("{module}.rs"));
    if file.is_file() {
        file
    } else {
        module_root.join(module)
    }
}

fn record_module_target(
    entries: &mut BTreeSet<String>,
    src: &Path,
    target: &Path,
) -> anyhow::Result<()> {
    if target.is_file() {
        record(entries, "file", src, target)?;
    } else if target.is_dir() {
        record(entries, "dir", src, target)?;
        collect_rust_files(entries, src, target)?;
    }
    Ok(())
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

#[test]
fn declared_test_modules_are_found_from_rust_syntax_and_scope() -> anyhow::Result<()> {
    let root = std::env::temp_dir().join(format!(
        "harness-test-placement-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos()
    ));
    let src = root.join("src");
    fs::create_dir_all(src.join("suite"))?;
    fs::create_dir_all(src.join("outer"))?;
    fs::create_dir_all(src.join("nested_path"))?;
    fs::write(
        src.join("lib.rs"),
        r#"
            # [ cfg ( test ) ]
            #[path = "arbitrary.rs"]
            pub(crate)    mod hidden;

            #[cfg_attr(test, path = "override.rs")]
            mod variant;

            #[cfg(test)]
            mod suite {
                mod cases;
            }

            mod outer {
                #[cfg(test)]
                pub(super) mod inner;
            }

            #[cfg(test)]
            mod nested_path {
                #[path = "renamed.rs"]
                mod leaf;
            }
        "#,
    )?;
    for path in [
        src.join("arbitrary.rs"),
        src.join("override.rs"),
        src.join("suite/cases.rs"),
        src.join("outer/inner.rs"),
        src.join("nested_path/renamed.rs"),
    ] {
        fs::write(path, "")?;
    }

    let result = scan_legacy_homes(&src);
    let cleanup = fs::remove_dir_all(&root);
    let entries = result?;
    cleanup?;
    assert_eq!(
        entries,
        [
            "file src/arbitrary.rs",
            "file src/nested_path/renamed.rs",
            "file src/outer/inner.rs",
            "file src/override.rs",
            "file src/suite/cases.rs",
        ]
        .into_iter()
        .map(ToOwned::to_owned)
        .collect()
    );
    Ok(())
}
