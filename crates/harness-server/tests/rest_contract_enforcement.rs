use std::{
    fs,
    path::{Path, PathBuf},
};

#[test]
fn rest_contract_clippy_guard_cannot_silently_disappear() -> anyhow::Result<()> {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let clippy = fs::read_to_string(manifest_dir.join("clippy.toml"))?;
    for disallowed in ["axum::Json", "axum::extract::Query", "axum::extract::Path"] {
        assert!(
            clippy.contains(disallowed),
            "clippy.toml must disallow {disallowed}"
        );
    }

    let lib = fs::read_to_string(manifest_dir.join("src/lib.rs"))?;
    assert!(
        lib.contains("cfg_attr(not(test), deny(clippy::disallowed_types))"),
        "production harness-server must deny clippy::disallowed_types"
    );

    let mut suppressions = Vec::new();
    collect_suppressions(&manifest_dir.join("src"), &manifest_dir, &mut suppressions)?;
    suppressions.sort();
    assert_eq!(
        suppressions,
        [
            "src/http/rest_contract.rs:#![allow(clippy::disallowed_types)]",
            "src/lib.rs:#![cfg_attr(test,allow(clippy::disallowed_types))]",
        ],
        "only the reviewed adapter and the test-only crate override may suppress the REST boundary lint"
    );
    Ok(())
}

fn collect_suppressions(
    directory: &Path,
    manifest_dir: &Path,
    suppressions: &mut Vec<String>,
) -> anyhow::Result<()> {
    for entry in fs::read_dir(directory)? {
        let path = entry?.path();
        if path.is_dir() {
            collect_suppressions(&path, manifest_dir, suppressions)?;
            continue;
        }
        if path.extension().and_then(|extension| extension.to_str()) != Some("rs") {
            continue;
        }
        let source = fs::read_to_string(&path)?;
        let compact = source
            .chars()
            .filter(|character| !character.is_whitespace())
            .collect::<String>();
        let relative = path
            .strip_prefix(manifest_dir)?
            .to_string_lossy()
            .replace('\\', "/");

        for attribute in attributes(&compact) {
            if lint_level_bodies(attribute).iter().any(|(_, body)| {
                body.split(',').map(str::trim).any(|lint| {
                    matches!(
                        lint,
                        "clippy::disallowed_types"
                            | "clippy::all"
                            | "clippy::restriction"
                            | "warnings"
                    )
                })
            }) {
                suppressions.push(format!("{relative}:{attribute}"));
            }
        }
    }
    Ok(())
}

fn attributes(source: &str) -> Vec<&str> {
    let mut attributes = Vec::new();
    let mut offset = 0;
    while let Some(relative_start) = source[offset..].find('#') {
        let start = offset + relative_start;
        let suffix = &source[start..];
        let prefix_len = if suffix.starts_with("#![") {
            3
        } else if suffix.starts_with("#[") {
            2
        } else {
            offset = start + 1;
            continue;
        };
        let mut depth = 1_u32;
        let mut end = None;
        for (relative_end, character) in suffix[prefix_len..].char_indices() {
            match character {
                '[' => depth += 1,
                ']' => {
                    depth -= 1;
                    if depth == 0 {
                        end = Some(start + prefix_len + relative_end);
                        break;
                    }
                }
                _ => {}
            }
        }
        let Some(end) = end else {
            attributes.push(&source[start..]);
            break;
        };
        attributes.push(&source[start..=end]);
        offset = end + 1;
    }
    attributes
}

fn lint_level_bodies(source: &str) -> Vec<(&'static str, &str)> {
    let mut bodies = Vec::new();
    for level in ["allow", "expect"] {
        let needle = format!("{level}(");
        let mut remainder = source;
        while let Some(start) = remainder.find(&needle) {
            let body_start = start + needle.len();
            let candidate = &remainder[body_start..];
            let mut depth = 1_u32;
            let mut body_end = None;
            for (offset, character) in candidate.char_indices() {
                match character {
                    '(' => depth += 1,
                    ')' => {
                        depth -= 1;
                        if depth == 0 {
                            body_end = Some(offset);
                            break;
                        }
                    }
                    _ => {}
                }
            }
            let Some(body_end) = body_end else {
                bodies.push((level, candidate));
                break;
            };
            bodies.push((level, &candidate[..body_end]));
            remainder = &candidate[body_end + 1..];
        }
    }
    bodies
}

#[test]
fn suppression_inventory_preserves_cfg_conditions() {
    let attributes = [
        "#![cfg_attr(test,allow(clippy::disallowed_types))]",
        "#![allow(clippy::disallowed_types)]",
        "#![cfg_attr(not(test),expect(clippy::all))]",
    ];
    let discovered = attributes
        .iter()
        .flat_map(|source| self::attributes(source))
        .collect::<Vec<_>>();
    assert_eq!(discovered, attributes);
}
