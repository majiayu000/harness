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
        ["src/http/rest_contract.rs"],
        "only the reviewed adapter may suppress the REST boundary lint"
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
        if source
            .lines()
            .any(|line| line.trim() == "#![allow(clippy::disallowed_types)]")
        {
            suppressions.push(
                path.strip_prefix(manifest_dir)?
                    .to_string_lossy()
                    .replace('\\', "/"),
            );
        }
    }
    Ok(())
}
