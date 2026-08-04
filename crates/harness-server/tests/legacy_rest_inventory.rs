use quote::ToTokens;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
};
use syn::visit::{self, Visit};

const LEGACY_WIRE_TYPES: &str = include_str!("fixtures/legacy_rest_wire_types.txt");
const LEGACY_INVENTORY: &str = include_str!("fixtures/legacy_rest_inventory.txt");

#[test]
fn legacy_rest_inventory_can_only_shrink() -> anyhow::Result<()> {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let expected_type_keys = LEGACY_WIRE_TYPES.lines().collect::<BTreeSet<_>>();
    let mut current = BTreeSet::new();

    for path in rust_sources(&manifest_dir.join("src"))? {
        let relative = inventory_relative_path(&path, &manifest_dir)?;
        let source = fs::read_to_string(&path)?;
        let syntax = syn::parse_file(&source)?;
        let aliases = legacy_json_aliases(&syntax, &relative)?;
        let mut visitor = InventoryVisitor {
            relative: &relative,
            aliases,
            expected_type_keys: &expected_type_keys,
            entries: &mut current,
        };
        visitor.visit_file(&syntax);
    }

    let expected = LEGACY_INVENTORY
        .lines()
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(ToOwned::to_owned)
        .collect::<BTreeSet<_>>();
    let additions = current.difference(&expected).cloned().collect::<Vec<_>>();
    assert!(
        additions.is_empty(),
        "legacy REST inventory may only shrink; move new DTOs to harness-protocol or restore the reviewed legacy shape. New entries:\n{}",
        additions.join("\n")
    );
    Ok(())
}

struct InventoryVisitor<'a> {
    relative: &'a str,
    aliases: BTreeSet<String>,
    expected_type_keys: &'a BTreeSet<&'a str>,
    entries: &'a mut BTreeSet<String>,
}

impl InventoryVisitor<'_> {
    fn record_type<T: ToTokens>(&mut self, name: &syn::Ident, item: &T) {
        let key = format!("{}::{name}", self.relative);
        if self.expected_type_keys.contains(key.as_str()) {
            self.entries
                .insert(format!("type {key} {}", fingerprint(item)));
        }
    }

    fn record_function<T: ToTokens>(&mut self, name: &syn::Ident, item: &T) {
        let tokens = item.to_token_stream().to_string();
        let identifiers = tokens
            .split(|character: char| !character.is_ascii_alphanumeric() && character != '_')
            .collect::<BTreeSet<_>>();
        if identifiers.contains("LegacyJson")
            || self
                .aliases
                .iter()
                .any(|alias| identifiers.contains(alias.as_str()))
        {
            self.entries.insert(format!(
                "function {}::{name} {}",
                self.relative,
                fingerprint(item)
            ));
        }
    }

    fn uses_legacy_json(&self, value: &impl ToTokens) -> bool {
        let tokens = value.to_token_stream().to_string();
        let identifiers = tokens
            .split(|character: char| !character.is_ascii_alphanumeric() && character != '_')
            .collect::<BTreeSet<_>>();
        identifiers.contains("LegacyJson")
            || self
                .aliases
                .iter()
                .any(|alias| identifiers.contains(alias.as_str()))
    }
}

impl Visit<'_> for InventoryVisitor<'_> {
    fn visit_item_struct(&mut self, item: &syn::ItemStruct) {
        if !is_test_only(&item.attrs) {
            self.record_type(&item.ident, item);
        }
    }

    fn visit_item_enum(&mut self, item: &syn::ItemEnum) {
        if !is_test_only(&item.attrs) {
            self.record_type(&item.ident, item);
        }
    }

    fn visit_item_fn(&mut self, item: &syn::ItemFn) {
        if !is_test_only(&item.attrs) {
            self.record_function(&item.sig.ident, item);
        }
    }

    fn visit_item_type(&mut self, item: &syn::ItemType) {
        assert!(
            !self.uses_legacy_json(&item.ty),
            "{}: type aliases for LegacyJson are forbidden because they hide legacy handler use sites",
            self.relative
        );
    }

    fn visit_impl_item_fn(&mut self, item: &syn::ImplItemFn) {
        if !is_test_only(&item.attrs) {
            self.record_function(&item.sig.ident, item);
        }
    }

    fn visit_item_macro(&mut self, item: &syn::ItemMacro) {
        if item.mac.path.is_ident("register_legacy_dtos") {
            for root in item.mac.tokens.to_string().split(',').map(str::trim) {
                if !root.is_empty() {
                    self.entries.insert(format!("registry {root}"));
                }
            }
        }
        visit::visit_item_macro(self, item);
    }

    fn visit_item_mod(&mut self, item: &syn::ItemMod) {
        if !is_test_only(&item.attrs) {
            visit::visit_item_mod(self, item);
        }
    }
}

fn legacy_json_aliases(file: &syn::File, relative: &str) -> anyhow::Result<BTreeSet<String>> {
    let mut aliases = BTreeSet::new();
    for item in &file.items {
        let syn::Item::Use(item) = item else {
            continue;
        };
        let tokens = item.to_token_stream().to_string();
        if tokens.contains("rest_contract") && tokens.contains('*') {
            anyhow::bail!("{relative}: glob imports from rest_contract are forbidden");
        }
        collect_legacy_aliases(&item.tree, &mut aliases);
    }
    Ok(aliases)
}

fn collect_legacy_aliases(tree: &syn::UseTree, aliases: &mut BTreeSet<String>) {
    match tree {
        syn::UseTree::Name(name) if name.ident == "LegacyJson" => {
            aliases.insert("LegacyJson".to_string());
        }
        syn::UseTree::Rename(rename) if rename.ident == "LegacyJson" => {
            aliases.insert(rename.rename.to_string());
        }
        syn::UseTree::Path(path) => collect_legacy_aliases(&path.tree, aliases),
        syn::UseTree::Group(group) => {
            for item in &group.items {
                collect_legacy_aliases(item, aliases);
            }
        }
        _ => {}
    }
}

fn is_test_only(attributes: &[syn::Attribute]) -> bool {
    attributes.iter().any(|attribute| {
        let is_test_attribute = attribute
            .path()
            .segments
            .last()
            .is_some_and(|segment| segment.ident == "test");
        let cfg = attribute
            .meta
            .to_token_stream()
            .to_string()
            .chars()
            .filter(|character| !character.is_whitespace())
            .collect::<String>();
        is_test_attribute || cfg == "cfg(test)"
    })
}

#[test]
fn only_exact_test_cfg_is_excluded_from_inventory() {
    for source in ["#[cfg(not(test))]", "#[cfg(any(test, feature = \"prod\"))]"] {
        let file = syn::parse_file(&format!("{source}\nfn fixture() {{}}"))
            .expect("cfg fixture should parse");
        let syn::Item::Fn(function) = &file.items[0] else {
            panic!("fixture should be a function");
        };
        let attribute = function.attrs[0].clone();
        assert!(!is_test_only(&[attribute]));
    }
    let attribute: syn::Attribute = syn::parse_quote!(#[cfg(test)]);
    assert!(is_test_only(&[attribute]));
}

#[test]
#[should_panic(expected = "type aliases for LegacyJson are forbidden")]
fn legacy_json_type_aliases_fail_closed() {
    let file = syn::parse_file(
        "use crate::http::rest_contract::LegacyJson as Json; type Hidden = Json<serde_json::Value>;",
    )
    .expect("fixture should parse");
    let aliases = legacy_json_aliases(&file, "fixture.rs").expect("aliases should resolve");
    let expected_type_keys = BTreeSet::new();
    let mut entries = BTreeSet::new();
    InventoryVisitor {
        relative: "fixture.rs",
        aliases,
        expected_type_keys: &expected_type_keys,
        entries: &mut entries,
    }
    .visit_file(&file);
}

fn fingerprint(value: &impl ToTokens) -> String {
    let digest = Sha256::digest(value.to_token_stream().to_string().as_bytes());
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn rust_sources(root: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let mut sources = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(directory)? {
            let path = entry?.path();
            if path.is_dir() {
                pending.push(path);
            } else if path.extension().and_then(|value| value.to_str()) == Some("rs") {
                sources.push(path);
            }
        }
    }
    sources.sort();
    Ok(sources)
}

fn inventory_relative_path(path: &Path, manifest_dir: &Path) -> anyhow::Result<String> {
    Ok(path
        .strip_prefix(manifest_dir)?
        .to_string_lossy()
        .replace('\\', "/"))
}
