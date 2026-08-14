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
    let mut visited_modules = BTreeSet::new();
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
                collect_declared_test_modules(
                    &mut entries,
                    src,
                    &path,
                    false,
                    &mut visited_modules,
                )?;
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
    inherited_test: bool,
    visited_modules: &mut BTreeSet<(PathBuf, bool)>,
) -> anyhow::Result<()> {
    let canonical_source = fs::canonicalize(source)?;
    if !visited_modules.insert((canonical_source, inherited_test)) {
        return Ok(());
    }
    let contents = fs::read_to_string(source)?;
    let syntax = syn::parse_file(&contents)?;
    let source_dir = source.parent().unwrap_or(src);
    collect_test_module_items(
        entries,
        src,
        source_dir,
        &module_root(source),
        &syntax.items,
        inherited_test,
        visited_modules,
    )
}

fn collect_test_module_items(
    entries: &mut BTreeSet<String>,
    src: &Path,
    path_dir: &Path,
    module_dir: &Path,
    items: &[Item],
    inherited_test: bool,
    visited_modules: &mut BTreeSet<(PathBuf, bool)>,
) -> anyhow::Result<()> {
    for item in items {
        let Item::Mod(module) = item else {
            continue;
        };
        let test_only = inherited_test || module.attrs.iter().any(is_test_cfg);
        if let Some((_, nested_items)) = &module.content {
            let configured_test_path = cfg_attr_test_path(&module.attrs);
            let test_path = configured_test_path
                .clone()
                .or_else(|| test_only.then(|| path_attribute(&module.attrs)).flatten());
            let nested_module_dir = test_path.as_ref().map_or_else(
                || module_dir.join(module.ident.to_string()),
                |path| path_dir.join(path),
            );
            let nested_test = test_only || configured_test_path.is_some();
            if nested_test {
                record_module_target(entries, src, &nested_module_dir)?;
            }
            collect_test_module_items(
                entries,
                src,
                &nested_module_dir,
                &nested_module_dir,
                nested_items,
                nested_test,
                visited_modules,
            )?;
            continue;
        }

        let configured_test_path = cfg_attr_test_path(&module.attrs);
        let target = active_test_path(&module.attrs, test_only)
            .map(|path| path_dir.join(path))
            .or_else(|| {
                test_only.then(|| resolve_module_path(module_dir, &module.ident.to_string()))
            });
        if let Some(target) = target {
            record_module_target(entries, src, &target)?;
            if target.is_file() {
                collect_declared_test_modules(
                    entries,
                    src,
                    &target,
                    test_only || configured_test_path.is_some(),
                    visited_modules,
                )?;
            }
        }
    }
    Ok(())
}

fn active_test_path(attributes: &[Attribute], test_only: bool) -> Option<PathBuf> {
    cfg_attr_test_path(attributes)
        .or_else(|| test_only.then(|| path_attribute(attributes)).flatten())
}

#[derive(Clone, Copy)]
struct CfgTruth {
    mentions_test: bool,
    can_be_true: bool,
    can_be_false: bool,
}

impl CfgTruth {
    const UNKNOWN: Self = Self {
        mentions_test: false,
        can_be_true: true,
        can_be_false: true,
    };

    fn all(values: impl Iterator<Item = Self>) -> Self {
        values.fold(
            Self {
                mentions_test: false,
                can_be_true: true,
                can_be_false: false,
            },
            |result, value| Self {
                mentions_test: result.mentions_test || value.mentions_test,
                can_be_true: result.can_be_true && value.can_be_true,
                can_be_false: result.can_be_false || value.can_be_false,
            },
        )
    }

    fn any(values: impl Iterator<Item = Self>) -> Self {
        values.fold(
            Self {
                mentions_test: false,
                can_be_true: false,
                can_be_false: true,
            },
            |result, value| Self {
                mentions_test: result.mentions_test || value.mentions_test,
                can_be_true: result.can_be_true || value.can_be_true,
                can_be_false: result.can_be_false && value.can_be_false,
            },
        )
    }

    fn not(self) -> Self {
        Self {
            mentions_test: self.mentions_test,
            can_be_true: self.can_be_false,
            can_be_false: self.can_be_true,
        }
    }
}

fn cfg_truth_with_test_enabled(meta: &Meta) -> CfgTruth {
    match meta {
        Meta::Path(path) if path.is_ident("test") => CfgTruth {
            mentions_test: true,
            can_be_true: true,
            can_be_false: false,
        },
        Meta::Path(_) | Meta::NameValue(_) => CfgTruth::UNKNOWN,
        Meta::List(list) => {
            let Ok(arguments) =
                list.parse_args_with(Punctuated::<Meta, Token![,]>::parse_terminated)
            else {
                return CfgTruth::UNKNOWN;
            };
            if list.path.is_ident("all") {
                CfgTruth::all(arguments.iter().map(cfg_truth_with_test_enabled))
            } else if list.path.is_ident("any") {
                CfgTruth::any(arguments.iter().map(cfg_truth_with_test_enabled))
            } else if list.path.is_ident("not") && arguments.len() == 1 {
                cfg_truth_with_test_enabled(&arguments[0]).not()
            } else {
                CfgTruth::UNKNOWN
            }
        }
    }
}

fn cfg_enables_test(meta: &Meta) -> bool {
    let truth = cfg_truth_with_test_enabled(meta);
    truth.mentions_test && truth.can_be_true
}

fn is_test_cfg(attribute: &Attribute) -> bool {
    attribute.path().is_ident("cfg")
        && attribute
            .parse_args::<Meta>()
            .is_ok_and(|meta| cfg_enables_test(&meta))
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
            if !cfg_enables_test(condition) {
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
    fs::create_dir_all(src.join("external"))?;
    fs::create_dir_all(src.join("hidden_home"))?;
    fs::create_dir_all(src.join("cfg_hidden"))?;
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

            #[cfg(test)]
            mod external;

            #[cfg(all(test))]
            #[path = "hidden_home"]
            mod remapped {
                mod cases;
            }

            #[cfg_attr(all(test), path = "cfg_hidden")]
            mod cfg_remapped {
                mod cases;
            }

            #[cfg(any(test, feature = "synthetic"))]
            #[path = "any_test.rs"]
            mod any_test;

            #[cfg(not(test))]
            #[path = "production_only.rs"]
            mod production_only;
        "#,
    )?;
    fs::write(src.join("external.rs"), "mod child;")?;
    for path in [
        src.join("arbitrary.rs"),
        src.join("override.rs"),
        src.join("suite/cases.rs"),
        src.join("outer/inner.rs"),
        src.join("nested_path/renamed.rs"),
        src.join("external/child.rs"),
        src.join("hidden_home/cases.rs"),
        src.join("cfg_hidden/cases.rs"),
        src.join("any_test.rs"),
        src.join("production_only.rs"),
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
            "dir src/cfg_hidden",
            "dir src/hidden_home",
            "dir src/nested_path",
            "dir src/suite",
            "file src/any_test.rs",
            "file src/cfg_hidden/cases.rs",
            "file src/external.rs",
            "file src/external/child.rs",
            "file src/hidden_home/cases.rs",
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
